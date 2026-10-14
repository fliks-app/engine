//! A remux run's served segments, assembled from its GOP files and growing audio files onto the
//! grid and the served timeline (`RemuxSegmentAssembler`); the decisions are `core::mp4`'s.
//! Spec: `docs/spec/runtime.md` section 9.
use fliks_engine_core::mp4::{
    AudioGroup, OpenedRun, RemuxAssemblyPlan, check_gop, gop_complete, gops_of, open_audio,
    open_run, parse_frame, plan_boundary_seconds, read_box_header, retime_fragments, seg_name,
    tail_boxes,
};
use fliks_engine_core::session::RemuxProgress;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Passes a segment may fail before the error is not a passing lock.
const SEGMENT_ATTEMPTS: u32 = 3;

struct Audio {
    /// 1-based served index: `<dir>/<index>/`, from ffmpeg's `a<index - 1>.mp4`.
    index: usize,
    file: PathBuf,
    dir: PathBuf,
    offset: u64,
    group: Option<AudioGroup>,
    failures: u32,
}

pub struct Assembler {
    plan: RemuxAssemblyPlan,
    dir: PathBuf,
    gop_dir: PathBuf,
    next: usize,
    run: Option<OpenedRun>,
    audio: Vec<Audio>,
    failures: u32,
    stopped: bool,
    opened_at: Option<Instant>,
    open_start: usize,
    paused: Duration,
    paused_since: Option<Instant>,
}

fn exists(p: &Path) -> bool {
    p.exists()
}

/// Writes `out` through a temp file unless it is there already: a segment another run built
/// holds the same samples on the same timeline, and may have been served.
fn write_new(out: &Path, fill: impl FnOnce(&mut File) -> Result<(), String>) -> Result<(), String> {
    if exists(out) {
        return Ok(());
    }
    let tmp = out.with_extension(format!("tmp-{}", crate::random_hex(4)));
    let res = File::create(&tmp)
        .map_err(|e| e.to_string())
        .and_then(|mut f| fill(&mut f))
        .and_then(|()| std::fs::rename(&tmp, out).map_err(|e| e.to_string()));
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// A top-level box of a fragment file: `(start, size, its bytes when a moof)`.
type TopBox = (u64, u64, Option<Vec<u8>>);

fn top_boxes(f: &mut File) -> Result<Vec<TopBox>, String> {
    let end = f.metadata().map_err(|e| e.to_string())?.len();
    let mut out = vec![];
    let mut start = 0u64;
    while start < end {
        let mut head = [0u8; 16];
        f.seek(SeekFrom::Start(start)).map_err(|e| e.to_string())?;
        let n = f.read(&mut head).map_err(|e| e.to_string())?;
        let h = read_box_header(&head[..n], 0, start as usize, end as usize)
            .ok_or_else(|| format!("malformed box at byte {start} of {end}"))?;
        let moof = if &h.kind == b"moof" {
            let mut b = vec![0u8; h.size];
            f.seek(SeekFrom::Start(start)).map_err(|e| e.to_string())?;
            f.read_exact(&mut b).map_err(|e| e.to_string())?;
            Some(b)
        } else {
            None
        };
        out.push((start, h.size as u64, moof));
        start += h.size as u64;
    }
    Ok(out)
}

fn read_moofs(p: &Path) -> Result<Vec<u8>, String> {
    let mut f = File::open(p).map_err(|e| e.to_string())?;
    Ok(top_boxes(&mut f)?
        .into_iter()
        .filter_map(|b| b.2)
        .flatten()
        .collect())
}

impl Assembler {
    /// `dir` receives `init.mp4`, `seg-NNNN.m4s` and `<i>/`; `gop_dir` is this run's ffmpeg output.
    pub fn new(plan: RemuxAssemblyPlan, dir: PathBuf, gop_dir: PathBuf) -> Self {
        let audio = (1..=plan.audio_renditions as usize)
            .map(|index| Audio {
                index,
                file: gop_dir.join(format!("a{}.mp4", index - 1)),
                dir: dir.join(index.to_string()),
                offset: 0,
                group: None,
                failures: 0,
            })
            .collect();
        Self {
            next: plan.start_segment,
            plan,
            dir,
            gop_dir,
            run: None,
            audio,
            failures: 0,
            stopped: false,
            opened_at: None,
            open_start: 0,
            paused: Duration::ZERO,
            paused_since: None,
        }
    }

    pub fn stopped(&self) -> bool {
        self.stopped
    }

    fn gop_path(&self, gop: usize) -> PathBuf {
        let offset = self.run.as_ref().map_or(0, |r| r.gop_offset);
        self.gop_dir
            .join(format!("gop-{}.m4s", gop as i64 - offset))
    }

    fn is_complete(&self, gop: usize) -> Result<bool, String> {
        let file = self.gop_path(gop);
        if !exists(&file) {
            return Ok(false);
        }
        let video = self.run.as_ref().and_then(|r| r.video);
        if self.plan.gop_dts.is_none() || self.plan.boundaries.is_none() || video.is_none() {
            return Ok(true);
        }
        gop_complete(&self.plan, video.as_ref(), gop, &read_moofs(&file)?)
            .map_err(|e| e.to_string())
    }

    /// One pass over what ffmpeg wrote; true when a segment landed. Gives up (`stopped`) after
    /// `SEGMENT_ATTEMPTS` failures in a row.
    pub fn pump(&mut self) -> bool {
        if self.stopped {
            return false;
        }
        let before = (self.next, self.audio_served());
        match self.pump_video() {
            Ok(true) => {
                for i in 0..self.audio.len() {
                    if self.stopped {
                        break;
                    }
                    self.pump_audio(i);
                }
            }
            Ok(false) => {}
            Err(e) => {
                self.failures += 1;
                if self.failures < SEGMENT_ATTEMPTS {
                    tracing::warn!(
                        segment = self.next,
                        "remux segment not assembled (attempt {}): {e}",
                        self.failures
                    );
                } else {
                    tracing::error!(segment = self.next, "remux assembly stopped: {e}");
                    self.stopped = true;
                }
            }
        }
        before != (self.next, self.audio_served())
    }

    fn audio_served(&self) -> Vec<Option<usize>> {
        self.audio
            .iter()
            .map(|a| a.group.as_ref().map(|g| g.served))
            .collect()
    }

    /// False until the run opened.
    fn pump_video(&mut self) -> Result<bool, String> {
        if self.run.is_none() && !self.open()? {
            return Ok(false);
        }
        while let Some(gops) = gops_of(&self.plan, self.next) {
            if !self.is_complete(*gops.last().unwrap_or(&self.next))? {
                break;
            }
            self.assemble(self.next, &gops)?;
            self.next += 1;
            self.failures = 0;
        }
        Ok(true)
    }

    /// Once the run's first GOP is out: its init is complete and where it landed is known.
    fn open(&mut self) -> Result<bool, String> {
        let first = self
            .gop_dir
            .join(format!("gop-{}.m4s", self.plan.start_number));
        if !exists(&first) {
            return Ok(false);
        }
        let init = std::fs::read(self.gop_dir.join("init.mp4")).map_err(|e| e.to_string())?;
        let run = open_run(&self.plan, &init, &read_moofs(&first)?).map_err(|e| e.to_string())?;
        if run.next != self.plan.start_segment || run.gop_offset != 0 {
            tracing::warn!(
                planned = self.plan.start_segment,
                from = run.next,
                "remux run landed on another GOP"
            );
        }
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        write_new(&self.dir.join("init.mp4"), |f| {
            f.write_all(&run.init).map_err(|e| e.to_string())
        })?;
        self.next = run.next;
        self.open_start = run.next;
        self.opened_at = Some(Instant::now());
        self.run = Some(run);
        self.failures = 0;
        Ok(true)
    }

    fn assemble(&self, segment: usize, gops: &[usize]) -> Result<(), String> {
        write_new(&self.dir.join(seg_name(segment)), |out| {
            gops.iter().try_for_each(|g| self.append_gop(out, *g))
        })?;
        for g in gops {
            let _ = std::fs::remove_file(self.gop_path(*g));
        }
        Ok(())
    }

    /// GOP `g` onto `out`: fragment headers retimed, media data copied through.
    fn append_gop(&self, out: &mut File, g: usize) -> Result<(), String> {
        let run = self.run.as_ref().ok_or("run not open")?;
        let mut src = File::open(self.gop_path(g)).map_err(|e| e.to_string())?;
        let mut checked = false;
        for (start, size, moof) in top_boxes(&mut src)? {
            let Some(moof) = moof else {
                src.seek(SeekFrom::Start(start))
                    .map_err(|e| e.to_string())?;
                let n =
                    std::io::copy(&mut (&mut src).take(size), out).map_err(|e| e.to_string())?;
                if n < size {
                    return Err(format!("fragment file ends {} bytes early", size - n));
                }
                continue;
            };
            if !checked {
                checked = check_gop(&self.plan, run.video.as_ref(), run.gop_offset, g, &moof)
                    .map_err(|e| e.to_string())?;
            }
            let retimed = retime_fragments(&moof, &run.deltas).map_err(|e| e.to_string())?;
            out.write_all(&retimed).map_err(|e| e.to_string())?;
        }
        if !checked {
            return Err(format!("GOP {g} has no video fragment"));
        }
        Ok(())
    }

    /// ffmpeg exited: a clean run also completes its last segment, whose last GOP is only then
    /// known whole. The GOP files go either way.
    pub fn finish(&mut self, clean: bool) {
        self.pump();
        if clean && !self.stopped {
            if let Err(e) = self.assemble_tail() {
                tracing::error!(segment = self.next, "last remux segment not assembled: {e}");
            }
            for i in 0..self.audio.len() {
                if let Err(e) = self.audio_tail(i) {
                    tracing::error!(rendition = i + 1, "audio rendition tail not assembled: {e}");
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.gop_dir);
    }

    fn assemble_tail(&mut self) -> Result<(), String> {
        let Some(gops) = gops_of(&self.plan, self.next).filter(|_| self.run.is_some()) else {
            return Ok(());
        };
        let mut written: Vec<usize> = gops
            .iter()
            .copied()
            .filter(|g| exists(&self.gop_path(*g)))
            .collect();
        // A clean exit proves nothing about the last GOP: a SIGTERM still writes the trailer.
        if let Some(last) = written.last()
            && !self.is_complete(*last)?
        {
            written.pop();
        }
        if written.is_empty() {
            return Ok(());
        }
        if written.len() < gops.len() {
            tracing::warn!(
                segment = self.next,
                "last remux segment: {} of {} planned GOPs were complete",
                written.len(),
                gops.len()
            );
        }
        self.assemble(self.next, &written)?;
        self.next += 1;
        Ok(())
    }

    fn pump_audio(&mut self, i: usize) {
        match self.audio_frames(i) {
            Ok(()) => self.audio[i].failures = 0,
            Err(e) => {
                let a = &mut self.audio[i];
                a.failures += 1;
                if a.failures < SEGMENT_ATTEMPTS {
                    tracing::warn!(
                        rendition = a.index,
                        "audio segment not assembled (attempt {}): {e}",
                        a.failures
                    );
                } else {
                    tracing::error!(rendition = a.index, "audio assembly stopped: {e}");
                    self.stopped = true;
                }
            }
        }
    }

    /// The rendition's track and delta from its growing file's init, once ffmpeg wrote one.
    fn open_audio(&mut self, i: usize) -> Result<bool, String> {
        let run = self.run.as_ref().ok_or("video run not resolved yet")?;
        let a = &self.audio[i];
        let Ok(buf) = std::fs::read(&a.file) else {
            return Ok(false);
        };
        let Some(moov) = tail_boxes(&buf, 0).into_iter().find(|b| &b.kind == b"moov") else {
            return Ok(false);
        };
        let (group, init) =
            open_audio(&buf[..moov.end()], a.index, run).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(&a.dir).map_err(|e| e.to_string())?;
        write_new(&a.dir.join(format!("init_{}.mp4", a.index)), |f| {
            f.write_all(&init).map_err(|e| e.to_string())
        })?;
        let a = &mut self.audio[i];
        a.offset = moov.end() as u64;
        a.group = Some(group);
        Ok(true)
    }

    fn audio_frames(&mut self, i: usize) -> Result<(), String> {
        if self.audio[i].group.is_none() && !self.open_audio(i)? {
            return Ok(());
        }
        let a = &mut self.audio[i];
        let mut f = File::open(&a.file).map_err(|e| e.to_string())?;
        f.seek(SeekFrom::Start(a.offset))
            .map_err(|e| e.to_string())?;
        let mut buf = vec![];
        f.read_to_end(&mut buf).map_err(|e| e.to_string())?;
        let boxes = tail_boxes(&buf, 0);
        let base = a.offset;
        let group = a.group.as_mut().ok_or("audio not open")?;
        for [moof, mdat] in boxes.as_chunks::<2>().0 {
            if &moof.kind != b"moof" || &mdat.kind != b"mdat" {
                break;
            }
            let frame = parse_frame(&buf[moof.start..moof.end()], &buf[mdat.start..mdat.end()])
                .map_err(|e| e.to_string())?;
            let Some(closed) = group.push(&self.plan, frame).map_err(|e| e.to_string())? else {
                break;
            };
            for (seg, bytes) in closed {
                write_new(&a.dir.join(seg_name(seg)), |f| {
                    f.write_all(&bytes).map_err(|e| e.to_string())
                })?;
            }
            a.offset = base + mdat.end() as u64;
        }
        Ok(())
    }

    fn audio_tail(&mut self, i: usize) -> Result<(), String> {
        if self.audio[i].group.is_none() {
            return Ok(());
        }
        self.audio_frames(i)?;
        let a = &mut self.audio[i];
        let Some(group) = a.group.as_mut() else {
            return Ok(());
        };
        if let Some((seg, bytes)) = group.flush().map_err(|e| e.to_string())? {
            write_new(&a.dir.join(seg_name(seg)), |f| {
                f.write_all(&bytes).map_err(|e| e.to_string())
            })?;
        }
        Ok(())
    }

    /// Segments per second since open, pauses excluded.
    fn segments_per_second(&self) -> Option<f64> {
        let at = self.opened_at?;
        let paused = self.paused + self.paused_since.map_or(Duration::ZERO, |p| p.elapsed());
        let secs = at.elapsed().saturating_sub(paused).as_secs_f64();
        (secs > 0.0).then(|| (self.next - self.open_start) as f64 / secs)
    }

    /// Progress of the video (`None`) or of rendition `audio` (1-based).
    pub fn progress(&self, audio: Option<usize>) -> RemuxProgress {
        let frontier = self.run.as_ref().and_then(|r| match audio {
            None => Some(self.next),
            Some(i) => self
                .audio
                .get(i.wrapping_sub(1))
                .map(|a| a.group.as_ref().map_or(r.next, |g| g.served)),
        });
        RemuxProgress {
            start_segment: self.plan.start_segment as u32,
            frontier: frontier.map(|f| f as u32),
            segments_per_second: self.segments_per_second(),
        }
    }

    /// Content seconds of segment `i` (source time less the plan's start).
    pub fn segment_content_seconds(&self, i: usize) -> f64 {
        plan_boundary_seconds(&self.plan, i) - self.plan.start
    }

    /// `(run start, frontier)` in content seconds, `None` before the run landed.
    pub fn span(&self) -> (Option<f64>, Option<f64>) {
        match self.run {
            Some(_) => (
                Some(self.segment_content_seconds(self.open_start)),
                Some(self.segment_content_seconds(self.next)),
            ),
            None => (None, None),
        }
    }

    /// Freezes or resumes the throughput clock with the throttle's pause.
    pub fn pause(&mut self, pause: bool) {
        match (pause, self.paused_since) {
            (true, None) => self.paused_since = Some(Instant::now()),
            (false, Some(p)) => {
                self.paused += p.elapsed();
                self.paused_since = None;
            }
            _ => {}
        }
    }
}

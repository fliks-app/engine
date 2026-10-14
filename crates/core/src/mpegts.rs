//! MPEG-TS demuxer (PAT/PMT/PES); today it feeds the scan: the ADTS payload of chosen PIDs, fed to one `AdtsConfigWalker` each.
//! Spec: `docs/spec/jobs-pure.md` section 1.
use crate::jobs::scan::AdtsConfigWalker;
use std::collections::HashMap;

const SYNC: u8 = 0x47;
const PACKET: usize = 188;
/// Plain, M2TS (4-byte timecode prefix) and Reed-Solomon (16-byte suffix) packet spacings.
const STRIDES: [usize; 3] = [188, 192, 204];
/// Sync bytes in a row that make a spacing credible.
const LOCK_PACKETS: usize = 4;
const STREAM_TYPE_ADTS: u8 = 0x0f;
const PES_HEADER: usize = 9;
const MAX_SECTION: usize = 1024;

#[derive(PartialEq)]
enum Seq {
    Next,
    Dup,
    Gap,
}

fn seq(last: &mut Option<u8>, cc: u8) -> Seq {
    let s = match *last {
        None => Seq::Next,
        Some(l) if cc == l => Seq::Dup,
        Some(l) if cc == (l + 1) & 15 => Seq::Next,
        Some(_) => Seq::Gap,
    };
    *last = Some(cc);
    s
}

struct Track {
    pid: u16,
    walker: AdtsConfigWalker,
    cc: Option<u8>,
    /// Payload is a PES tail whose start was lost: dropped until the next payload_unit_start.
    wait: bool,
    broken: bool,
    hdr: Vec<u8>,
    /// Header bytes left to drop once the fixed 9 are read.
    skip: Option<usize>,
}

impl Track {
    fn lose(&mut self) {
        self.wait = true;
        self.walker.resync();
    }

    fn feed(&mut self, mut d: &[u8], pusi: bool, cc: u8, scrambled: bool) {
        match seq(&mut self.cc, cc) {
            Seq::Dup => return,
            Seq::Gap => self.lose(),
            Seq::Next => {}
        }
        if scrambled {
            self.broken = true;
        }
        if pusi {
            self.hdr.clear();
            self.skip = None;
            self.wait = false;
        }
        if self.wait || self.broken {
            return;
        }
        if self.skip.is_none() {
            let n = (PES_HEADER - self.hdr.len()).min(d.len());
            self.hdr.extend_from_slice(&d[..n]);
            d = &d[n..];
            if self.hdr.len() < PES_HEADER {
                return;
            }
            if self.hdr[..3] != [0, 0, 1] {
                self.broken = true;
                return;
            }
            self.skip = Some(usize::from(self.hdr[8]));
        }
        let skip = self.skip.as_mut().unwrap();
        let n = (*skip).min(d.len());
        *skip -= n;
        if n < d.len() {
            self.walker.push(&d[n..]);
        }
    }
}

#[derive(Default)]
struct Psi {
    buf: Vec<u8>,
    cc: Option<u8>,
}

/// Follows PAT and PMT, reassembles the PES of the asked PIDs and walks their ADTS frames.
/// A track is unreadable (`None` at `finish`) when its PID carries something else than ADTS
/// (PMT stream type, scrambling, no PES header) or no frame at all.
#[derive(Default)]
pub struct Demuxer {
    buf: Vec<u8>,
    stride: Option<usize>,
    tracks: Vec<Option<Track>>,
    psi: HashMap<u16, Psi>,
}

impl Demuxer {
    /// One slot per entry of `pids`; `None` is a track whose PID is unknown.
    pub fn new(pids: &[Option<u16>]) -> Self {
        Self {
            tracks: pids
                .iter()
                .map(|p| {
                    p.map(|pid| Track {
                        pid,
                        walker: AdtsConfigWalker::default(),
                        cc: None,
                        wait: true,
                        broken: false,
                        hdr: vec![],
                        skip: None,
                    })
                })
                .collect(),
            psi: HashMap::from([(0, Psi::default())]),
            ..Self::default()
        }
    }

    pub fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
        self.drain(false);
    }

    /// The walker of each track given to `new`, in order; the demuxer is spent.
    pub fn finish(&mut self) -> Vec<Option<AdtsConfigWalker>> {
        self.drain(true);
        std::mem::take(&mut self.tracks)
            .into_iter()
            .map(|t| t.filter(|t| !t.broken && t.walker.seen()).map(|t| t.walker))
            .collect()
    }

    /// Offset and spacing of the first run of sync bytes; else where to resume (`eof`: a run cut by
    /// the end of data counts).
    fn lock(&self, from: usize, eof: bool) -> Result<(usize, usize), usize> {
        let b = &self.buf;
        for o in from..b.len() {
            if b[o] != SYNC {
                continue;
            }
            let mut wait = false;
            for s in STRIDES {
                let mut ok = true;
                for k in 1..LOCK_PACKETS {
                    match b.get(o + k * s) {
                        Some(&SYNC) => {}
                        Some(_) => {
                            ok = false;
                            break;
                        }
                        None if eof => break,
                        None => {
                            wait = true;
                            ok = false;
                            break;
                        }
                    }
                }
                if ok && o + PACKET <= b.len() {
                    return Ok((o, s));
                }
            }
            if wait {
                return Err(o);
            }
        }
        Err(b.len())
    }

    fn drain(&mut self, eof: bool) {
        let mut i = 0;
        loop {
            let Some(s) = self.stride else {
                match self.lock(i, eof) {
                    Ok((o, s)) => (i, self.stride) = (o, Some(s)),
                    Err(keep) => {
                        i = keep;
                        break;
                    }
                }
                continue;
            };
            if !(i + s <= self.buf.len() || (eof && i + PACKET <= self.buf.len())) {
                break;
            }
            if self.buf[i] != SYNC {
                self.stride = None;
                self.tracks.iter_mut().flatten().for_each(|t| {
                    t.cc = None;
                    t.lose();
                });
                self.psi.values_mut().for_each(|p| *p = Psi::default());
                continue;
            }
            let p: [u8; PACKET] = self.buf[i..i + PACKET].try_into().unwrap();
            self.packet(&p);
            i += s;
        }
        self.buf.drain(..i.min(self.buf.len()));
    }

    fn packet(&mut self, p: &[u8; PACKET]) {
        // Transport error: the packet is not to be trusted.
        if p[1] & 0x80 != 0 {
            return;
        }
        let pusi = p[1] & 0x40 != 0;
        let pid = u16::from(p[1] & 0x1f) << 8 | u16::from(p[2]);
        let (scrambled, afc, cc) = (p[3] >> 6 != 0, p[3] >> 4 & 3, p[3] & 15);
        let (mut off, mut disc) = (4, false);
        if afc & 2 != 0 {
            let n = usize::from(p[4]);
            disc = n > 0 && p[5] & 0x80 != 0;
            off = 5 + n;
        }
        let payload = (afc & 1 != 0 && off < PACKET).then(|| &p[off..]);
        if let Some(t) = self.tracks.iter_mut().flatten().find(|t| t.pid == pid) {
            if disc {
                t.cc = None;
                t.lose();
            }
            if let Some(d) = payload {
                t.feed(d, pusi, cc, scrambled);
            }
        } else if let Some(d) = payload {
            self.section(pid, d, pusi, cc);
        }
    }

    /// PSI payloads of the PAT and the PMTs it names, reassembled into sections.
    fn section(&mut self, pid: u16, d: &[u8], pusi: bool, cc: u8) {
        let Some(ps) = self.psi.get_mut(&pid) else {
            return;
        };
        match seq(&mut ps.cc, cc) {
            Seq::Dup => return,
            Seq::Gap => ps.buf.clear(),
            Seq::Next => {}
        }
        if pusi {
            let start = 1 + usize::from(d[0]);
            ps.buf.clear();
            if start < d.len() {
                ps.buf.extend_from_slice(&d[start..]);
            }
        } else if !ps.buf.is_empty() {
            ps.buf.extend_from_slice(d);
        }
        // 0xff: stuffing after the last section.
        if ps.buf.len() < 3 || ps.buf[0] == 0xff {
            return;
        }
        let total = 3 + (usize::from(ps.buf[1] & 0x0f) << 8 | usize::from(ps.buf[2]));
        if total > MAX_SECTION {
            ps.buf.clear();
        } else if ps.buf.len() >= total {
            let mut sec = std::mem::take(&mut ps.buf);
            sec.truncate(total);
            // current_next_indicator: a table about to apply is not this one.
            if total >= 16 && sec[5] & 1 != 0 {
                match (pid, sec[0]) {
                    (0, 0) => self.pat(&sec),
                    (_, 2) => self.pmt(&sec),
                    _ => {}
                }
            }
        }
    }

    fn pat(&mut self, s: &[u8]) {
        for e in s[8..s.len() - 4].as_chunks::<4>().0 {
            if e[0] | e[1] != 0 {
                let pid = u16::from(e[2] & 0x1f) << 8 | u16::from(e[3]);
                self.psi.entry(pid).or_default();
            }
        }
    }

    /// A tracked PID the PMT now gives another stream type is not ADTS any more.
    fn pmt(&mut self, s: &[u8]) {
        let end = s.len() - 4;
        let mut i = 12 + (usize::from(s[10] & 0x0f) << 8 | usize::from(s[11]));
        while i + 5 <= end {
            let pid = u16::from(s[i + 1] & 0x1f) << 8 | u16::from(s[i + 2]);
            if s[i] != STREAM_TYPE_ADTS
                && let Some(t) = self.tracks.iter_mut().flatten().find(|t| t.pid == pid)
            {
                t.broken = true;
            }
            i += 5 + (usize::from(s[i + 3] & 0x0f) << 8 | usize::from(s[i + 4]));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUDIO: u16 = 0x101;
    const PMT: u16 = 0x1000;

    /// A 7-byte-header ADTS frame of `len` bytes; `sf` the sampling index.
    fn frame(sf: u8, len: usize) -> Vec<u8> {
        let mut f = vec![0xff, 0xf1, 0x40 | sf << 2, 0x80 | (len >> 11) as u8 & 3];
        f.extend([(len >> 3) as u8, (len << 5) as u8 | 0x1f, 0xfc]);
        f.resize(len, 0xaa);
        f
    }

    fn pes(es: &[u8], header_extra: usize) -> Vec<u8> {
        let mut p = vec![0, 0, 1, 0xc0, 0, 0, 0x80, 0x80, header_extra as u8];
        p.extend(vec![0x21; header_extra]);
        p.extend_from_slice(es);
        p
    }

    struct Mux {
        cc: HashMap<u16, u8>,
    }

    impl Mux {
        fn new() -> Self {
            Self { cc: HashMap::new() }
        }

        /// Packets of `payload`, the first with payload_unit_start; the last one stuffed by an
        /// adaptation field.
        fn packets(&mut self, pid: u16, pusi: bool, payload: &[u8]) -> Vec<Vec<u8>> {
            let mut out = vec![];
            for (n, c) in payload.chunks(184).enumerate() {
                let cc = self.cc.entry(pid).or_insert(15);
                *cc = (*cc + 1) & 15;
                let mut p = vec![
                    SYNC,
                    u8::from(pusi && n == 0) << 6 | (pid >> 8) as u8,
                    pid as u8,
                ];
                if c.len() < 184 {
                    let stuff = 183 - c.len();
                    p.push(0x30 | *cc);
                    p.push(stuff as u8);
                    if stuff > 0 {
                        p.push(0);
                        p.extend(vec![0xff; stuff - 1]);
                    }
                } else {
                    p.push(0x10 | *cc);
                }
                p.extend_from_slice(c);
                assert_eq!(p.len(), 188);
                out.push(p);
            }
            out
        }

        fn psi(&mut self, pid: u16, table: &[u8]) -> Vec<Vec<u8>> {
            let mut s = vec![0];
            s.extend_from_slice(table);
            self.packets(pid, true, &s)
        }

        fn pat(&mut self) -> Vec<Vec<u8>> {
            let mut t = vec![0, 0xb0, 13, 0, 1, 0xc1, 0, 0, 0, 1];
            t.extend([0xe0 | (PMT >> 8) as u8, PMT as u8, 0, 0, 0, 0]);
            self.psi(0, &t)
        }

        fn pmt(&mut self, streams: &[(u8, u16)]) -> Vec<Vec<u8>> {
            let len = 9 + 5 * streams.len() + 4;
            let mut t = vec![2, 0xb0 | (len >> 8) as u8, len as u8, 0, 1, 0xc1, 0, 0];
            t.extend([0xe1, 0, 0xf0, 0]);
            for (ty, pid) in streams {
                t.extend([*ty, 0xe0 | (pid >> 8) as u8, *pid as u8, 0xf0, 0]);
            }
            t.extend([0; 4]);
            self.psi(PMT, &t)
        }

        fn audio(&mut self, es: &[u8]) -> Vec<Vec<u8>> {
            self.packets(AUDIO, true, &pes(es, 5))
        }
    }

    fn run(chunks: &[Vec<u8>], packet_split: usize) -> Option<AdtsConfigWalker> {
        let mut d = Demuxer::new(&[Some(AUDIO), None]);
        for c in chunks.concat().chunks(packet_split) {
            d.push(c);
        }
        let mut w = d.finish();
        assert!(w[1].is_none());
        w.remove(0)
    }

    fn head(m: &mut Mux) -> Vec<Vec<u8>> {
        [m.pat(), m.pmt(&[(0x1b, 0x100), (STREAM_TYPE_ADTS, AUDIO)])].concat()
    }

    #[test]
    fn a_pes_split_across_packets_and_a_config_change_are_seen() {
        let mut m = Mux::new();
        let mut t = head(&mut m);
        let same: Vec<u8> = (0..30).flat_map(|_| frame(4, 300)).collect();
        t.extend(m.audio(&same));
        let w = run(&t, 1 << 20).unwrap();
        assert!(!w.changed);
        // The next PES holds a frame of another sampling rate, cut mid-frame by a packet boundary.
        t.extend(m.audio(&[frame(4, 300), frame(3, 500)].concat()));
        for split in [188, 1, 777, 1 << 20] {
            assert!(run(&t, split).unwrap().changed, "split {split}");
        }
    }

    #[test]
    fn a_frame_spanning_two_pes_is_one_frame() {
        let mut m = Mux::new();
        let mut t = head(&mut m);
        let f = frame(4, 400);
        t.extend(m.audio(&[f.clone(), f[..150].to_vec()].concat()));
        t.extend(m.audio(&f[150..]));
        t.extend(m.audio(&f));
        assert!(!run(&t, 4096).unwrap().changed);
    }

    #[test]
    fn adaptation_only_packets_and_a_long_pes_header_do_not_disturb() {
        let mut m = Mux::new();
        let mut t = head(&mut m);
        let af = |cc: u8| {
            let mut p = vec![SYNC, (AUDIO >> 8) as u8, AUDIO as u8, 0x20 | cc, 183, 0x10];
            p.resize(188, 0xff);
            p
        };
        // PCR-only packets carry the counter of the last payload packet, not the next one.
        t.push(af(0));
        t.extend(m.packets(AUDIO, true, &pes(&frame(4, 200), 200)));
        t.push(af(m.cc[&AUDIO]));
        t.extend(m.audio(&frame(3, 200)));
        assert!(run(&t, 4096).unwrap().changed);
    }

    #[test]
    fn a_duplicate_packet_is_dropped_and_a_gap_drops_the_pes_it_cuts() {
        let mut m = Mux::new();
        let mut t = head(&mut m);
        t.extend(m.audio(&frame(4, 200)));
        let mut dup = m.audio(&frame(4, 200));
        dup.push(dup[0].clone());
        t.extend(dup);
        assert!(!run(&t, 4096).unwrap().changed);
        // A 3-packet PES loses its middle packet: its tail is junk and must not count, the next PES does.
        let big: Vec<u8> = frame(3, 380).into_iter().chain(frame(3, 380)).collect();
        let mut p = m.packets(AUDIO, true, &pes(&big, 5));
        assert_eq!(p.len(), 5);
        p.remove(2);
        t.extend(p);
        t.extend(m.audio(&frame(4, 200)));
        assert!(!run(&t, 4096).unwrap().changed);
        let w = run(&[t.clone(), m.audio(&frame(3, 200))].concat(), 4096).unwrap();
        assert!(w.changed);
    }

    #[test]
    fn a_pmt_giving_the_pid_another_type_makes_the_track_unreadable() {
        let mut m = Mux::new();
        let mut t = head(&mut m);
        t.extend(m.audio(&frame(4, 200)));
        assert!(run(&t, 4096).is_some());
        // An update on a PMT that still lists it as ADTS, then one that does not.
        t.extend(m.pmt(&[(STREAM_TYPE_ADTS, AUDIO)]));
        assert!(run(&t, 4096).is_some());
        t.extend(m.pmt(&[(0x11, AUDIO)]));
        assert!(run(&t, 4096).is_none());
    }

    #[test]
    fn a_pmt_split_over_two_packets_is_followed() {
        let mut m = Mux::new();
        let mut t = m.pat();
        let mut streams: Vec<(u8, u16)> = (0..40).map(|i| (0x06, 0x200 + i)).collect();
        streams.push((0x03, AUDIO));
        let p = m.pmt(&streams);
        assert_eq!(p.len(), 2);
        t.extend(p);
        t.extend(m.audio(&frame(4, 200)));
        assert!(run(&t, 4096).is_none());
    }

    #[test]
    fn lost_sync_resumes_at_the_next_run_of_packets() {
        let mut m = Mux::new();
        let mut t = head(&mut m);
        for _ in 0..20 {
            t.extend(m.audio(&frame(4, 200)));
        }
        // Junk and a cut packet between two good stretches; the PES start after it is found again.
        t.push(vec![0x13; 100]);
        t.push(vec![SYNC; 5]);
        for _ in 0..20 {
            t.extend(m.audio(&frame(3, 200)));
        }
        assert!(run(&t, 4096).unwrap().changed);
        assert!(run(&t, 187).unwrap().changed);
    }

    #[test]
    fn m2ts_and_reed_solomon_spacings_are_found() {
        let mut m = Mux::new();
        let mut t = head(&mut m);
        for _ in 0..10 {
            t.extend(m.audio(&[frame(4, 200), frame(3, 200)].concat()));
        }
        let m2ts: Vec<u8> = t
            .iter()
            .flat_map(|p| [&[0u8, 0, 0x12, 0x34][..], p].concat())
            .collect();
        let rs: Vec<u8> = t
            .iter()
            .flat_map(|p| [&p[..], &[0u8; 16][..]].concat())
            .collect();
        for (name, bytes) in [("m2ts", m2ts), ("204", rs)] {
            let w = run(&[bytes], 5000).unwrap();
            assert!(w.changed, "{name}");
        }
    }

    #[test]
    fn a_track_with_no_frame_or_scrambled_is_unreadable() {
        let mut m = Mux::new();
        let t = head(&mut m);
        assert!(run(&t, 4096).is_none());
        let mut t = head(&mut m);
        let mut a = m.audio(&frame(4, 200));
        a[0][3] |= 0x80;
        t.extend(a);
        assert!(run(&t, 4096).is_none());
    }

    #[test]
    fn a_pes_without_a_start_code_is_unreadable() {
        let mut m = Mux::new();
        let mut t = head(&mut m);
        t.extend(m.audio(&frame(4, 200)));
        t.extend(m.packets(AUDIO, true, &[9; 50]));
        assert!(run(&t, 4096).is_none());
    }
}

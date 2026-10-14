use utoipa::OpenApi;

fn main() {
    println!(
        "{}",
        fliks_engine::ApiDoc::openapi().to_pretty_json().unwrap()
    );
}

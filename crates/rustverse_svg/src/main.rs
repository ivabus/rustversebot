use rustverse_svg::render_template_source;

fn main() {
    let template_path = std::env::args().nth(1).unwrap();
    let data_path = std::env::args().nth(2).unwrap();
    let template = std::fs::read_to_string(&template_path).unwrap();
    let data: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(data_path).unwrap()).unwrap();

    std::fs::write(
        template_path.replace("j2", "png"),
        render_template_source(&template, &data),
    )
    .unwrap();
}

use glsl::parser::Parse;
use glsl::syntax::ShaderStage;
use glsl::transpiler::glsl::show_translation_unit;
use glsl::visitor::HostMut;
use wgpu_mc_jni::preprocessing::MatrixPatcher;
fn main() {
    let mut vert_stage = ShaderStage::parse(
        r#"uniform vec2 Fog;
uniform sampler2D Sampler;
uniform vec2 Projection;

void main() {
}"#,
    )
    .unwrap();

    vert_stage.visit_mut(&mut MatrixPatcher);

    let mut out = String::new();

    show_translation_unit(&mut out, &vert_stage);

    println!("## Vert ##\n{out}\n");
}

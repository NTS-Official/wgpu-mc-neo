# "wgpu-mc" — Minecraft Rendering Engine Built in Rust
<img  src="assets/logo.png" width="280" alt="Logo">
<p>
<img alt="Static Badge" src="https://img.shields.io/badge/Powered_by-WebGPU-orange?logo=webgpu&logoSize=auto&link=https%3A%2F%2Fwebgpu.org%2F">
<img alt="Static Badge" src="https://img.shields.io/badge/Discord_-5865F2?style=flat-square&logo=discord&logoColor=fff&link=https%3A%2F%2Fdiscord.gg%2FNTuK8bQ2hn">
<img alt="Static Badge" src="https://img.shields.io/badge/Matrix_-000?style=flat-square&logo=matrix&logoColor=fff&link=%20https%3A%2F%2Fmatrix.to%2F%23%2F%23wgpu-mc%3Amatrix.org">
<img alt="Dynamic Badge" src="https://img.shields.io/github/actions/workflow/status/NTS-Official/wgpu-mc-neo/build.yml">
</p>

> [!WARNING]
> Original project wgpu-mc is in **Beta**. [Contributions appreciated](https://github.com/wgpu-mc/wgpu-mc/labels/engine).<br>

**wgpu-mc** is a standalone [WebGPU](https://www.w3.org/TR/webgpu/) rendering engine written in Rust using the [`wgpu`](https://gpuweb.github.io/gpuweb/) crate. The project was started in late 2021 as a pet project to create a new rendering engine for Minecraft to replace the existing OpenGL renderer.
## About this "wgpu-mc-neo" project
#### This is the unofficial **neoforge** branch of wgpu-mc. 
> [!WARNING]
> Since the original project was far from meeting the goals, this project revamped and modified its Rust side. **So it will no longer be a simply migrating subproject of wgpu-mc**. For maintainability and the author's availability, this project has removed the Fabric mod part.

The author's original intention was to introduce ray tracing/path tracing and modern graphics technologies like DLSS, DLSSD, and DLSSR under DirectX in such an experimental project. However, due to the limitations of the crate WebGPU, currently they can only be implemented through pretty dirty methods like hooking in C++ libraries. So, this project will no longer consider adding those technologies and will instead focus on improving the practicality and stability of this cross-API graphics project.

## Neolectrum — Rust-based Rendering Engine Mod for Minecraft
> [!CAUTION]
> Electrum is currently wip. [Feel free to contribute!](https://github.com/wgpu-mc/wgpu-mc/labels/electrum).

#### **Neolectrum** is a Neoforge mod that integrates the wgpu-mc rendering engine, replacing the existing Blaze3D rendering engine. 

By hijacking the original GL renderer at the start of the game through a mixin, and using Rust's complete GLSL to WGSL translation process along with a WebGPU-based rendering pipeline, we can replace Minecraft's rendering backend with DirectX12, Vulkan, Metal, or even more backends. (Currently there are only Vulkan and DirectX12 available.)
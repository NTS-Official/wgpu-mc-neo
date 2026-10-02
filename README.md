# "wgpu-mc" — Minecraft Rendering Engine Built in Rust
<img  src="assets/logo.png" width="280" alt="Logo">

**English** | [简体中文](README_zh.md)
<p>
<img alt="Static Badge" src="https://img.shields.io/badge/Powered_by-WebGPU-orange?logo=webgpu&logoSize=auto&link=https%3A%2F%2Fwebgpu.org%2F">
<img alt="Static Badge" src="https://img.shields.io/badge/Discord_-5865F2?style=flat-square&logo=discord&logoColor=fff&link=https%3A%2F%2Fdiscord.gg%2FNTuK8bQ2hn">
<img alt="Static Badge" src="https://img.shields.io/badge/Matrix_-000?style=flat-square&logo=matrix&logoColor=fff&link=%20https%3A%2F%2Fmatrix.to%2F%23%2F%23wgpu-mc%3Amatrix.org">
<img alt="Dynamic Badge" src="https://img.shields.io/github/actions/workflow/status/NTS-Official/wgpu-mc-neo/build.yml">
</p>

> [!WARNING]
> wgpu-mc is in **Beta**. [Contributions appreciated](https://github.com/NTS-Official/wgpu-mc-neo/labels/wgpu-mc).<br>

**wgpu-mc** is a standalone [WebGPU](https://www.w3.org/TR/webgpu/) rendering engine written in Rust using the [`wgpu`](https://gpuweb.github.io/gpuweb/) crate. The project was started in late 2021 as a pet project to create a new rendering engine for Minecraft to replace the existing OpenGL renderer.
## About this "wgpu-mc-neo" project
#### This is the unofficial **neoforge** branch of wgpu-mc. 
> [!NOTE]
> Since the original project was far from meeting the goals, this project revamped and modified its Rust side. **So it will no longer be a simply migrating subproject of wgpu-mc**. For maintainability and the author's availability, this project has removed the Fabric mod part.

The author's original intention was to introduce ray tracing/path tracing and modern graphics technologies like DLSS, DLSSD, and DLSSR under DirectX in such an experimental project. However, due to the limitations of the crate WebGPU, currently they can only be implemented through pretty dirty methods like hooking in C++ libraries. So, this project will no longer consider adding those technologies and will instead focus on improving the practicality and stability of this cross-API graphics project.

## Neolectrum — Rust-based Rendering Engine Mod for Minecraft
> [!NOTE]
> **The mod is a repository of its own now: [NTS-Official/Neolectrum](https://github.com/NTS-Official/Neolectrum).** It is built and released there, and it takes this engine's native library out of a checkout beside it - `../wgpu-mc-neo/rust` by default, or wherever its `wgpu_mc_rust_dir` points.

> [!CAUTION]
> Neolectrum is currently in **alpha**. [Feel free to contribute!](https://github.com/NTS-Official/wgpu-mc-neo/labels/neolectrum).

#### **Neolectrum** is a Neoforge mod that integrates the wgpu-mc rendering engine, replacing the existing Blaze3D rendering engine. 

By hijacking the original GL renderer at the start of the game through a mixin, and using Rust's complete GLSL to WGSL translation process along with a WebGPU-based rendering pipeline, we can replace Minecraft's rendering backend with DirectX12, Vulkan, Metal, or even more backends. (Currently there are only Vulkan and DirectX12 available.)

## Acknowledgments
[VulkanMod](https://github.com/xCollateral/VulkanMod)、[Radiance](https://github.com/Minecraft-Radiance/Radiance) & [MCVR](https://github.com/Minecraft-Radiance/MCVR)、[C2ME](https://github.com/RelativityMC/C2ME-fabric)、[Bye-Pregen](https://github.com/MoePus/Bye-Pregen) were referred as some extraodinary examples of implementation in this project. Tribute to these developers who contribute to community selflessly!
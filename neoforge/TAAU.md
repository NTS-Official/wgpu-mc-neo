# TAAU 实施笔记

目标：在这个渲染器里实现时序抗锯齿 + 上采样（TAAU）——内部以低于输出的分辨率渲染，靠每帧亚像素抖动与 history 累积在**输出分辨率**重建图像；GUI 不做时序处理。

本文件记录**已核实的落点**与阶段划分，供后续每一轮直接接着做。

---

## 一、已核实的关键落点

### 1.1 抖动（jitter）的注入点：ProjectionMatrixBuffer

MC 26.1 用 `net.minecraft.client.renderer.ProjectionMatrixBuffer` 统一承载投影 UBO，
每个用途一个实例、名字不同（`ProjectionMatrixBuffer.java:23-29`，label = "Camera projection matrix UBO " + name）：

| name | 创建处 | 每帧调用处 | 是否要抖动 |
|---|---|---|---|
| `level` | `GameRenderer.java:142` | `GameRenderer.java:721` | **是** |
| `3d hud` | `GameRenderer.java:143` | `GameRenderer.java:747` | **是**（与关卡同一空间） |
| `cubemap` | `CubeMap.java:39` | 天空盒 | **是**（后续处理，天空无有效速度） |
| `gui` | `GuiRenderer.java:103` | `GuiRenderer.java:210` | 否 |
| `post` | `ShaderManager.java:57` | `ShaderManager.java:246` | 否 |
| `items` | `GuiItemAtlas.java:42` | GUI 物品图集 | 否 |
| `PIP - *` | `PictureInPictureRenderer.java:31` | 画中画 | 否 |

另外 `ParticleFeatureRenderer.java:89` 直接 `setUniform("Projection", ...)`，沿用的仍是关卡那块 buffer。

写入路径：`ProjectionMatrixBuffer.writeBuffer`（`:49-56`）→ `Std140Builder.putMat4f` → `RenderSystem.getDevice().createCommandEncoder().writeToBuffer(this.buffer.slice(), byteBuffer)`（`:52`）。也就是走 mod 自己实现的 `writeToBuffer`。

**陷阱（必须处理）**：`getBuffer(Projection)` 在投影与版本号都没变时直接 return、不重写（`:34-35`）。相机静止时投影不变 → 若只在 `writeBuffer` 里注入抖动，那一帧不会写入新抖动，TAA 就收不到新的子像素样本。

**结论——注入方案**：

- `@Mixin(ProjectionMatrixBuffer.class)`，用一个 `@Unique` 字段在 `<init>` 里捕获 `name`，按上表标记哪些是"世界投影"；
- 在 `getBuffer(Lnet/minecraft/client/renderer/Projection;)` 的 RETURN 处注入：拿方法参数 `Projection`，`projection.getMatrix(tmp)` 得到**未抖动**的基准矩阵，把 `jitter(tmp, 本帧亚像素偏移)` 写入该 buffer 的 slice（用 mod 自己的 `writeToBuffer`）；
  - 这样每帧都写，不依赖 MC 的 dirty 检查；也不需要 `@Invoker` 或 `@Redirect`。
- 每帧推进抖动序列放在已有的 `GameRendererMixin`（`GameRendererMixin.java:33-36`，`@Inject(method = "render", at = HEAD)`）——它已经是"每帧一次"的既有位置。
- **地形必须用同一个抖动值**：`TerrainPass.sendCameraMatrices` 里投影只在一处合成——`TerrainPass.kt:198` 的 `Matrix4f(cameraState.projectionMatrix).mul(bob.last().pose()).get(projection)`，在这一行之后施加**同一个**像素空间抖动，再交给 `WgpuNative.setMatrix(MATRIX_PROJECTION, projection)`（`:255`）。两处用同一套 JOML 操作，保证"地形与实体被抖到同一亚像素位置"。

**为什么不能只抖地形**：MC 自己的绘制（实体、粒子、方块实体）与地形若偏移不同，TAA 会把差异当成运动 → 撕裂/拖影。所以抖动必须在两处一致。

### 1.2 mod 侧已有的、TAAU 直接可用的设施

| 设施 | 位置 | 对 TAAU 的用途 |
|---|---|---|
| 每帧一次的钩子 | `GameRendererMixin.java:33` | 推进抖动序列、帧计数 |
| 关卡共享 encoder 与一帧一次提交 | `create_command_encoder`（SHARED_ENCODER）、`blit_from_texture`（`device.rs:5342`） | 插入 resolve pass；history 拷贝 |
| **地形 pass 的附件由 mod 决定** | `render_terrain_pass`（`device.rs:4416`） | **挂速度附件，零额外 draw call** |
| 纹理到纹理拷贝 | `WgpuCommandEncoder.kt:896` | 保存上一帧深度 |
| 图集的 mip view 与每级动画 | `create_texture_view`（`device.rs:1354`）、`TextureAtlas.java:233-247` | 了解 atlas 侧不受 TAA 影响 |
| 每个 MC shader 都被 AST 重写 | `preprocessing.rs` | 后续给 MC 几何注入速度输出 |
| 动画贴图的现成标记位 | `UV_GAME_ATLAS`（`pipeline.rs:43`） | TAA resolve 里给这些像素**更短 history**（水/熔岩/火/草的运动不是几何运动，不处理必然 ghost） |
| buffer label 可读 | `WgpuBuffer.kt:27`，且已有按 label 匹配的先例（`:141`） | 抖动注入的 name 门控 |
| 后处理管线可挂进图 | `graph.rs:900-966`、`graph.yaml` | TAA resolve pass |

### 1.3 已知的尺寸语义陷阱

`ensure_scene`（`device.rs:5361-5365`）现在用 **swapchain** 尺寸建场景 depth。一旦引入渲染缩放（内部尺寸 ≠ 输出尺寸），关卡 color 是内部尺寸、depth 却是输出尺寸，地形 pass 的附件尺寸会不匹配。**做阶段 1 时必须同时改掉这里。**

---

## 二、阶段划分

- **阶段 0（前置）**：记录 `atlas_base_mip_only = true` 的临时规避状态——TAA 会把"高层 mip 陈旧"的闪烁表现变成**拖影/暗斑**，更难查；另外评估 `device.rs:535` 无条件开着的 `VALIDATION | DEBUG`（新管线一旦不匹配会直接结束进程，迭代成本高）。
- **阶段 1（渲染缩放，本身即 SSAA）**：`render_scale` 设置；在 mod 上报帧缓冲尺寸那一处乘 scale；修 `ensure_scene`；present 改 **2×2 四抽**盒式降采样。验证：尺寸正确、无拉伸、GUI 布局正常、depth 与 color 同尺寸。
- **阶段 2（抖动 + 上一帧矩阵）**：按 §1.1 实现；新增 uniform `jitter` / `prev_view_proj` / `inv_view_proj`（用**未抖动**矩阵存历史）。验证：相机与场景都静止时画面出现**极轻微、逐帧变化**的抖动；只有地形抖说明 chokepoint 没覆盖全。
- **阶段 3（速度缓冲，先只覆盖地形）**：在 `render_terrain_pass` 额外挂速度附件；`terrain.wgsl` / `terrain_solid.wgsl` 增 `@location(1) velocity`（`prev_view_proj * worldPos` 与当前 clip 之差），天空输出哨兵值。验证：把速度当颜色画——平移整体均匀、旋转随距离渐变、静止纯中灰。
- **阶段 4（TAA resolve，scale = 1.0）**：`graph.yaml` 新增 `@post_taa`（当前色 + depth + velocity + history + sampler + uniforms）；history 输出尺寸 ping-pong；失效钩子挂在 resize、`ensure_scene` 重建 depth、换世界/维度（`clearSections`/`forgetAll`）、缩放/预设变化；邻域 clamp；**动画贴图用 `UV_GAME_ATLAS` 做 mask**；**插在 GUI pass 之前**，present 保持 1:1。验证：静止相机时抖动被累积掉；移动无拖影。
- **阶段 5（打开上采样）**：内部降到 67%/77%，resolve 直接写输出尺寸；加上采样档位与轻量 sharpen。验证：与阶段 1 的 SSAA 做"清晰度/闪烁/拖影"三轴对比。
- **阶段 6（MC 自身几何的速度）**：用 `preprocessing.rs` 给打到关卡颜色的 MC shader 注入 velocity；在 `create_render_pass` 里自动附加速度附件；动态物体先接受近似，靠邻域 clamp 兜底。

## 三、必须提前定下的三个决策

1. **内部尺寸语义**：让 MC 认为帧缓冲变小（推荐，投影/视口/GUI 自动一致）还是只缩小关卡目标（GUI 与关卡分辨率不一致，易出布局问题）。
2. **TAA 插在哪**：GUI 之前（世界时序化、GUI 锐利，推荐）还是 present 之前（GUI 一起被时序化，文字会糊）。
3. **动态物体速度**：先只做相机运动（`prevViewProj * worldPos`），接受快速移动实体轻微 ghosting；还是上"逐 draw 的上一帧模型矩阵"（匹配 draw 成本高）。

## 四、风险

- 抖动 chokepoint 的**顺序**是硬约束：本帧抖动必须在 `getBuffer` 返回之后、关卡绘制之前写进 buffer；且地形与 MC 必须用同一个值。
- **TAAU 不解决当前的 CPU 瓶颈**：draw call 数一个不少，省的是 GPU 像素、提升的是画质。帧率上限仍取决于 draw call 那件事。
- **TAA 与"高层 mip 陈旧"互相掩盖**：`atlas_base_mip_only = true` 的规避在 TAA 下会变成拖影。
- **MSAA 与 TAA 互斥**；SSAA 与 TAAU 共用渲染缩放设施，阶段 1 的投入不浪费。

### 1.4 实现细节（编译前必须知道）

- `ProjectionMatrixBuffer.bufferSlice` 是**包级私有**字段，而 mixin 在 `dev.birb.wgpu.mixin.render` 包 → 需要 `@Accessor("bufferSlice")` 才能拿到那块 slice。
- `getBuffer` 有**两个重载**（`Projection` 与 `Matrix4f`），mixin 的 `method` 必须写完整描述符
  `getBuffer(Lnet/minecraft/client/renderer/Projection;)Lcom/mojang/blaze3d/buffers/GpuBufferSlice;`
  才能定位到带 `Projection` 的那个。
- 写抖动所需的 `writeToBuffer` **已经实现**：`WgpuCommandEncoder.kt:279-280` → `WmNative.writeToBuffer`。
- 每个 `ProjectionMatrixBuffer` 实例每帧只调用一次 `getBuffer(Projection)`（`GameRenderer.java:721` / `:747`），
  所以"每帧在 RETURN 处写一次抖动"与 MC 的调用节奏一一对应，不需要额外的帧计数器。
- `Projection#getMatrix(Matrix4f)` 是公开的（`ProjectionMatrixBuffer.java:39` 自己就在用），
  所以在 RETURN 注入点可以直接取到**未抖动**的基准矩阵。

---

## 五、进度

### 阶段 2 的代码已落地（开关默认关）

新增：

- `neoforge/src/main/kotlin/dev/birb/wgpu/render/TaaJitter.kt`
  Halton(2,3) 十六点序列、本帧像素偏移、按投影名字判定、`apply`（改 `m20`/`m21`）、
  渲染尺寸取 `Window#getWidth/getHeight`（返回的是 `framebufferWidth/Height`，正是渲染目标像素数）。
- `neoforge/src/main/java/dev/birb/wgpu/mixin/render/ProjectionMatrixBufferMixin.java`
  构造时记住 name；在 `getBuffer(Projection)` 的 RETURN 处，用 `Projection#getMatrix` 取**未抖动**基准矩阵，
  施加抖动后写回**返回值那块 slice**（返回值就是 `bufferSlice`，所以不需要 `@Accessor`）。

改动：

- `wgpu_mc.mixins.json`：注册上面的 mixin。
- `GameRendererMixin`：`render` HEAD 处 `TaaJitter.advanceFrame()`（每帧推进采样）。
- `TerrainPass.kt` 的投影合成处：走同一个 `TaaJitter.apply`，保证与 MC 自己那份一致。

**开法**（三选一）：运行目录下建一个名为 `wgpu-taa` 的文件；或 `-Dwgpu_mc.taa=true`；或 `WGPU_MC_TAA=1`。

**验证信号**：相机与世界都静止时，画面出现**极轻微、逐帧变化**的抖动。

- 只有地形在抖 → chokepoint 没覆盖全（MC 那份没写上）；
- GUI 文字在抖 → 名字过滤错了（`gui` 不该在 `WORLD_PROJECTIONS` 里）；
- 完全不抖 → 抖动没进任何一份投影（先看开关是否真的读到）。

**注意**：只有抖动、还没有 TAA resolve 时，画面看起来会**更差**（轻微振动）——这是阶段 2 的验收
信号，不是回归，所以默认关。

### 本环境的验证边界

沙箱下 **Gradle 无法启动**：wrapper 要在 `C:\Users\astra\.gradle\wrapper\dists\...` 取锁，该路径在工作区
之外被拒绝（`FileNotFoundException ... 拒绝访问`），网络也不可达。所以 JVM 侧**编译与运行必须由开发者
这边做**。已对着反编译源码逐个核对的 API：`Projection` 的包名、`Projection#getMatrix(Matrix4f)` 的可见性、
`Window#getWidth/getHeight` 返回 `framebufferWidth/Height`、`RenderSystem.PROJECTION_MATRIX_UBO_SIZE`、
`RenderSystem#getDevice().createCommandEncoder().writeToBuffer(...)`，以及 mod 侧
`WgpuCommandEncoder.writeToBuffer` 用的是 `data.remaining()`（64 字节正好是 UBO 大小）。

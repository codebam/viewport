# viewport-vulkan

A Vulkan renderer for [Smithay](https://github.com/Smithay/smithay)-based
Wayland compositors.

Smithay ships GLES, glow and pixman renderers and no Vulkan one. Its
`backend::vulkan` module says as much:

> This module does not provide abstractions for logical devices, rendering or
> memory allocation.

What is there — instance creation, physical device enumeration, DRM node
matching, format modifier queries — is the foundation this builds on. wlroots
has had a Vulkan renderer for years; this exists so a Smithay compositor can
have one too.

## Status

Early, but it renders. `smithay::desktop::space::render_output` can drive it.
Everything below is tested against real hardware, reading results back through
a CPU mapping of the rendered buffer rather than trusting the GPU.

- [x] Device selection by DRM node
- [x] DRM fourcc ↔ Vulkan format mapping, modifier queries
- [x] Images from imported DMA-BUFs, with foreign-queue acquire barriers
- [x] Command submission and clears, via dynamic rendering
- [x] Pipelines and shaders — textured and solid quads, premultiplied blending
- [x] `Renderer`, `Frame`, `Bind`, `ImportDma`
- [x] `ImportMem` — shm clients, via a staging buffer
- [x] `ExportMem` — read-back for screenshots and screencopy
- [x] Output and surface transforms
- [x] Explicit sync: `finish()` returns an exported `sync_file` fence
- [ ] Waiting on an imported fence in the queue rather than on the CPU
- [x] `Offscreen` (with an allocator) and `Blit`
- [ ] `ImportDmaWl` / `ImportMemWl` — the `wl_buffer` wrappers
- [ ] Colour management

## Why Vulkan

Colour is the honest answer. Compositing HDR surfaces next to SDR ones means
applying a transfer function and primaries per surface on the way to a shared
output space. Under GLES that is a fragment shader per combination plus
driver-specific guesswork about what an imported texture actually contains.
Under Vulkan the format, modifier and colour space of an imported image are
stated explicitly, which is the difference between implementing
`color-management-v1` and approximating it.

## Using it

```rust
use smithay::backend::drm::DrmNode;

// The same GPU the rest of the compositor allocates from. Importing a client's
// DMA-BUF into a device on another card either fails or copies over PCIe.
let node = DrmNode::from_path("/dev/dri/renderD128")?;
let device = viewport_vulkan::open(&node)?;

println!("{} on queue family {}", device.name(), device.queue_family());
```

`Device::for_node` takes an existing `Instance` where one is already owned.

## Requirements

Vulkan 1.2 or newer, plus:

`VK_KHR_external_memory_fd`, `VK_EXT_external_memory_dma_buf`,
`VK_EXT_image_drm_format_modifier`, `VK_KHR_image_format_list`,
`VK_EXT_queue_family_foreign`, `VK_KHR_external_semaphore_fd`.

All of these exist to move images between APIs and processes without a copy.

## Testing

The GPU tests skip where there is no render node, and a skip is indistinguishable
from a pass:

```
VIEWPORT_REQUIRE_GPU=1 cargo test -p viewport-vulkan
```

That turns every skip into a failure. CI should set it.

## Shaders

SPIR-V is committed rather than compiled at build time. The shaders are three
files that change rarely, and compiling them would put a C++ toolchain and
shaderc into the dependency graph of everyone who builds this. Each shader's
header comment carries the line that regenerates it:

```
glslangValidator -V shaders/quad.vert -o shaders/quad.vert.spv
```

There is no vertex buffer. The quad is generated from `gl_VertexIndex` as a
four-vertex triangle strip and positioned entirely from push constants, so
drawing a surface is a push and a draw with nothing to allocate or
synchronise. Textures are bound with `VK_KHR_push_descriptor`, which removes
the descriptor pool a compositor would otherwise have to size and recycle
every frame.

Blending is premultiplied — `ONE`, not `SRC_ALPHA`. Wayland buffers are
premultiplied, and using `SRC_ALPHA` double-multiplies, which shows up as dark
halos around translucent edges.

## Licence

MIT.

Not headed upstream: Smithay's `AI.md` asks that AI-generated contributions be
disclosed and advises against them, and this was written with AI assistance.
MIT anyway, because the reference implementation worth reading is wlroots'
Vulkan renderer, which is MIT, and because a renderer is reusable in a way the
rest of a compositor is not.

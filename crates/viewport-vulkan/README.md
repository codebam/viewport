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

Early. Device selection and format handling work and are tested against real
hardware. The renderer traits are not implemented yet.

- [x] Device selection by DRM node
- [x] DRM fourcc ↔ Vulkan format mapping, modifier queries
- [x] Images from imported DMA-BUFs, with foreign-queue acquire barriers
- [ ] `Renderer`, `Frame`, `Bind`
- [ ] `ImportDma`, `ImportMem`
- [ ] Explicit sync via timeline semaphores
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

## Licence

MIT.

Not headed upstream: Smithay's `AI.md` asks that AI-generated contributions be
disclosed and advises against them, and this was written with AI assistance.
MIT anyway, because the reference implementation worth reading is wlroots'
Vulkan renderer, which is MIT, and because a renderer is reusable in a way the
rest of a compositor is not.

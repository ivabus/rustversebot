# GPU renderer migration plan

Status: design and test-harness foundation
Reference renderer: `resvg` 0.47 through `rustverse_svg`
Target text renderer: `glyphon` 0.12 / `wgpu` 30 API generation
Parity reference scale: `zoom_factor = 1.0`
Compatibility default scale: `zoom_factor = 5.0`

## 1. Goal

Replace the CPU-bound SVG/template/rasterization path in `rustverse_svg` with a
headless GPU renderer while preserving the appearance and behavior of every
card currently emitted by the bot.

The final production path must:

- build a typed, layered scene rather than an SVG document;
- render all text through `glyphon`;
- decode every image and upload it to a GPU texture before drawing;
- create one persistent font/glyph atlas set and one persistent image atlas set
  during renderer startup, then reuse them for the renderer lifetime;
- support the shape, paint, clipping, masking, image-fitting, text, blending,
  and ordering features used by all current templates;
- expose an extensible effect chain for layer-local effects such as fill,
  masks, opacity, color transforms, blur, and future custom effects;
- expose a separate backdrop-effect chain that can sample already-rendered
  content, including a bounded backdrop blur;
- accept a render scale for every render;
- preserve the existing default output scale of 5.0 unless configuration
  explicitly changes it;
- compare the new renderer with the current renderer pixel by pixel at scale
  1.0;
- keep a single reusable GPU device, queue, pipeline set, glyph atlas set,
  image atlas set, effect registry, and transient-target pool instead of
  creating GPU state per card;
- return an ordinary PNG to Telegram-facing code.

This is a renderer replacement, not an SVG implementation. There will be no
SVG parser, CSS cascade, XML, DOM, or general-purpose browser paint model in
the final engine.

## 2. Non-goals

- General SVG compatibility.
- Arbitrary CSS or HTML layout.
- A live window, swap chain, or desktop UI. Rendering is headless.
- Runtime network access from the renderer. Downloading remains an async
  resource-loading concern outside GPU command encoding.
- Recreating or reuploading font/image atlases for individual renders.
- Duplicating Nanoka or HoYoLAB models in a new crate.
- Keeping MiniJinja as an intermediate graphics language after migration.
- Moving all work to the GPU. Data preparation, image decoding, text shaping,
  PNG encoding, and GPU readback necessarily contain CPU work. The expensive
  full-canvas rasterization and compositing are the GPU target.
- Supporting arbitrary vector paths before the current cards require them.
  The engine can add path tessellation later without being blocked by an SVG
  contract.

## 3. Current render surface

### 3.1 Public card families

The current crate renders seven variants:

| Public entry point | Template | Logical size |
| --- | --- | --- |
| `top_da` | `top_da.j2` | `450 x (144 + 65*n)` |
| `top_shiyu` | `top_shiyu.j2` | `384 x (144 + 65*n)` |
| `da` | `da.j2` | `640 x 360` view box |
| `shiyu` | `shiyu.j2` | `640 x 360` view box |
| `deadly_info` | `deadly_info.j2` | `512 x dynamic` |
| `deadly_info_with_begin_time` | `deadly_info.j2` | `512 x dynamic` |
| `shiyu_info` | `shiyu_info.j2` | `512 x dynamic` |

The player-result templates declare internal content as tall as 480 while
using a 360-high view box. The reference fixtures must capture the actual
clipping behavior before that discrepancy is changed.

### 3.2 Current feature inventory

| Existing feature | Where it is used | Required GPU representation |
| --- | --- | --- |
| Stable painter's order | Every template | Ordered scene nodes and stable flattening |
| Nested local coordinate systems | Room and leaderboard groups | Transform stack / group nodes |
| Opaque black clear | Every card | Render-pass clear |
| Solid fills | Backgrounds and masks | Primitive fragment shader |
| Rounded rectangles | All card panels | Analytic signed-distance primitive |
| Circles | Avatar rings and dot pattern | Analytic signed-distance primitive |
| Thin strokes | Panels, avatar rings, text | Shape stroke band; text mask stroke |
| Multi-stop linear gradients | Panels, strokes, ratings, text | Gradient paint with arbitrary sorted stops |
| Repeating patterns | Background and card dots | Procedural pattern paint or repeated texture |
| Rotated repeated image pattern | Hollows background | Texture sampler plus transform |
| Raster images | Logos, stars, bosses, agents, buffs | Persistent image-atlas regions |
| `cover` image fitting | Boss and avatar art | UV transform with centered crop |
| Linear image sampling | All resized art | Explicit sampler |
| Rounded clipping | Boss art and avatars | Clip stack / alpha mask |
| Alpha masks | Background and boss-art fades | Offscreen mask pass and mask composition |
| Horizontal and vertical fades | Boss art | Gradient mask |
| `multiply` composition | Legacy art mask | Multiply blend/composite pass |
| Source-over alpha | Every overdraw | Baseline blend state |
| Opacity | Gradient stops and masks | Premultiplied/straight-alpha policy |
| Text shaping and rasterization | Every card | `glyphon` |
| Text anchors | Start, center, end | Measured text bounds and anchor offset |
| Baseline alignment | Titles, scores, labels | Explicit baseline conversion |
| Multiline text | Mechanics and buffs | Existing wrapping plus `cosmic-text` buffers |
| Inline color spans | Game `<color=...>` tags | Styled text spans |
| Gradient text fill | Default text and ratings | Glyphon-produced mask plus paint composite |
| Text outline | Nearly every text style | Glyph mask expansion plus outline composite |
| Deterministic font | `inpin.ttf` | Font bytes loaded explicitly into `FontSystem` |
| Layer-local effects | Masks/fills are current building blocks | Extensible effect chain |
| Backdrop effects | Not currently used | Bounded backdrop capture and filter chain |
| Dynamic canvas height | Season information cards | Scene builder calculates logical bounds |
| Watermark styling | Every card | Shared text style |

The current templates use linear gradients only. The paint model should also
include radial gradients before `resvg` removal because circles and future
cards should not require a pipeline redesign. Radial-gradient implementation
is not allowed to delay linear-gradient parity.

## 4. Definition of parity

Feature parity has four separate gates. A card is not considered migrated
until it passes all four.

### 4.1 Structural parity

- Same logical canvas size.
- Same physical size for the same scale.
- Same visible strings and deterministic line breaks.
- Same images, image crop mode, and image placement.
- Same layer order, clips, masks, and blend modes.
- Same dynamic room/card height calculations.
- No unresolved image source can reach draw encoding.
- Layer effects run in declared order and cannot reorder ordinary content.
- Backdrop effects see only content painted before their layer.
- A normal render creates no atlas and performs no upload for startup-resident
  assets or prewarmed glyphs.

### 4.2 Pixel comparison at scale 1.0

The reference `resvg` path and GPU path render the same local fixture with
`RenderScale::ONE`. Both outputs are normalized to straight RGBA8 in row-major
order. The comparator reports:

- differing-pixel count after the configured channel threshold;
- per-channel differing counts;
- per-channel maximum absolute delta;
- total and mean absolute delta;
- RMS delta;
- the minimal bounding box containing every differing pixel;
- `expected.png`, `actual.png`, `diff.png`, and `report.txt` on failure.

The comparison default is exact: channel delta 0 and differing-pixel budget 0.
A temporary tolerance is allowed only for a named fixture and documented
rasterization discrepancy. It must not hide missing text, missing art, a
layout shift, clipping, or an incorrect blend mode. The intended end state is
an exact gate for primitives and the smallest reviewed edge-only budget for
text if glyphon and `resvg` rasterize the same font differently.

Pixel comparison is deliberately fixed to scale 1.0. Tests at other scales
validate scale invariants and layout, but are not substitutes for the 1.0
reference comparison.

### 4.3 Scale parity

Every scene is specified in logical units. `zoom_factor` is an input to each
render, not global mutable state and not a post-render resize.

Required scale rules:

1. `zoom_factor` must be finite and greater than zero.
2. Physical width and height are
   `round(logical_dimension * zoom_factor)`, matching the reference renderer.
3. Vertices, radii, stroke widths, mask extents, pattern transforms, and text
   rasterization all use the same scale.
4. Text wraps in logical units. Changing scale must not reflow a card.
5. Glyphon buffers use logical font metrics and logical wrapping bounds;
   `TextArea::scale` applies the physical scale.
6. Scissor bounds use floor for the minimum edge and ceil for the maximum edge
   so fractional scales do not remove edge pixels.
7. A render is rejected before allocation when its physical size exceeds
   `max_texture_dimension_2d` or the configured byte budget.
8. The compatibility default remains 5.0.
9. The minimum test matrix is `0.5, 1.0, 1.25, 2.0, 5.0`.
10. Scale can be selected per render. A bot-level default may be loaded from
    configuration, but it must not be compiled into shaders or pipelines.

### 4.4 Operational parity

- A render failure is isolated to its request.
- Remote image download remains async.
- Slow work does not block the Tokio core worker pool.
- Telegram receives PNG bytes with the same public call behavior during
  rollout.
- Device loss and out-of-memory errors are surfaced with context.
- A missing GPU produces an explicit startup/runtime error or invokes an
  intentional rollout fallback; it never silently returns a blank card.

## 5. Proposed architecture

```text
Nanoka / HoYoLAB models
          |
          v
existing prepare_* view logic
          |
          v
typed card scene builder
          |
          v
Scene (logical units, layered tree, asset handles)
          |
          +------------------------------+
          | validation and flattening    |
          | resource resolution/upload   |
          | effect/backdrop compilation  |
          | render-graph construction    |
          +------------------------------+
          |
          v
headless wgpu renderer
  - primitive pipelines
  - persistent image atlas set
  - persistent glyphon atlas set
  - clip/mask/composite pipelines
  - extensible effect pipelines
  - backdrop capture/filter pipelines
  - glyphon text pipelines
          |
          v
RGBA8 offscreen texture
          |
          v
256-byte-aligned staging buffer
          |
          v
unpadded RGBA8 bytes -> PNG encoder
```

During migration, the current crate should host both backends so that data
preparation is not copied:

```text
rustverse_svg
  src/model/            shared view models and prepare_* functions
  src/scene/            backend-neutral scene types
  src/cards/            card-specific scene builders
  src/gpu/              wgpu/glyphon backend
  src/gpu/atlas/        startup-owned image and glyph atlas state
  src/gpu/effects/      effect registry and render-graph compilers
  src/reference_svg/    temporary MiniJinja/resvg backend
  src/renderer.rs       public renderer contract
```

After cutover, remove `reference_svg`, MiniJinja, `resvg`, and the `.j2`
templates. Renaming the package to `rustverse_render` can be a final,
mechanical workspace change; it is not required for the renderer design and
must not happen mid-migration.

## 6. Public API contract

The target API should make expensive shared state and scale explicit:

```rust
pub struct Renderer {
    // Device, queue, pipelines, one glyph atlas set, one image atlas set,
    // effect registry, caches, and transient targets.
}

pub struct RenderOptions {
    pub scale: RenderScale,
    pub clear_color: Color,
}

pub struct RenderedImage {
    pub logical_size: LogicalSize,
    pub physical_size: PhysicalSize,
    pub rgba: Vec<u8>,
}

impl Renderer {
    pub async fn new(
        options: RendererInitOptions,
        startup_assets: StartupAssetManifest,
    ) -> Result<Self>;
    pub async fn render(
        &mut self,
        scene: &Scene,
        options: RenderOptions,
    ) -> Result<RenderedImage>;
    pub async fn render_png(
        &mut self,
        scene: &Scene,
        options: RenderOptions,
    ) -> Result<Vec<u8>>;
}
```

`Renderer::new` is the only normal constructor for GPU atlases. Startup is a
two-step transaction:

1. create device, queue, persistent atlases, pipelines, samplers, and effect
   registry;
2. load/upload every asset in the startup manifest and prewarm the configured
   glyph set.

The renderer becomes available to the request queue only after both steps
succeed. No partially populated renderer is published.

`RenderScale` is a validated positive finite newtype. It owns:

- physical-dimension calculation;
- logical-to-physical scalar conversion;
- scissor rounding helpers;
- allocation-limit checks.

It must not own card layout. Scene builders always use logical units.

The bot should not lock a `Renderer` across arbitrary async work. Use a
dedicated renderer service:

```text
caller -> bounded mpsc request -> renderer task -> oneshot response
```

The renderer task owns mutable glyphon and cache state. The bounded channel
provides backpressure. Image download/decode happens before a render request
or in a separately bounded resource stage.

## 7. Scene model

Use a retained layered tree for authoring and diagnostics, then flatten it to
a validated display list for encoding.

Minimum node model:

```rust
pub struct Scene {
    pub logical_size: LogicalSize,
    pub root: Layer,
}

pub struct Layer {
    pub debug_name: &'static str,
    pub transform: Transform2D,
    pub opacity: f32,
    pub blend_mode: BlendMode,
    pub clip: Option<Clip>,
    pub effects: Vec<Effect>,
    pub backdrop_effects: Vec<BackdropEffect>,
    pub content: LayerContent,
}

pub enum LayerContent {
    Group(Vec<Layer>),
    Shape(ShapeNode),
    Image(ImageNode),
    Text(TextNode),
    Mask {
        content: Box<Layer>,
        mask: Box<Layer>,
        operation: MaskOperation,
    },
}
```

Required details:

- `debug_name` is included in validation and GPU error context.
- Transforms are affine 2D transforms in logical coordinates.
- Opacity is clamped/validated to `[0, 1]`.
- Child order is paint order; flattening must be stable.
- Clips nest and intersect.
- Masks allocate a transient offscreen target only for their bounded region,
  not automatically for the full canvas.
- `effects` consume the layer's own rendered result in list order.
- `backdrop_effects` consume a snapshot of destination content painted before
  the layer. They cannot observe later siblings.
- Display-list commands retain source-layer IDs so a parity failure can be
  mapped back to a card component.
- Scene validation detects invalid sizes, non-finite coordinates, empty
  gradients, unsorted stops, invalid image handles, and unbalanced stacks
  before command encoding.

The scene API must not expose wgpu textures, bind groups, or pipelines. An
effect describes intent and parameters; the effect registry compiles it to
render-graph passes. This keeps card builders stable when an effect
implementation changes.

## 8. Geometry and primitive pipeline

The current cards can be covered efficiently with instanced analytic
primitives:

- rectangle;
- rounded rectangle;
- circle/ellipse;
- quad/triangle for procedural background details.

Each instance contains:

- local bounds;
- transform;
- corner radii or circle parameters;
- fill paint ID;
- optional stroke paint ID and logical width;
- opacity;
- clip/mask reference;
- z/order index used only for stable batching diagnostics.

The fragment shader evaluates signed distance for antialiased fill and stroke.
Antialias coverage must be based on derivatives in physical pixels so it
remains stable across scale.

Batch only consecutive compatible commands. Reordering opaque-looking nodes
is forbidden unless an equivalence proof exists because masks and blending
make painter's order observable.

No MSAA should be enabled in the first parity implementation. Analytic
coverage plus a single-sample target is more deterministic for readback.
MSAA can be benchmarked after parity and would require new goldens.

## 9. Paints and gradients

Minimum paint types:

```rust
pub enum Paint {
    Solid(Color),
    LinearGradient(LinearGradient),
    RadialGradient(RadialGradient),
    Pattern(PatternPaint),
}
```

Gradient requirements:

- at least two stops;
- stop offsets are finite, within `[0, 1]`, and nondecreasing;
- duplicate offsets are allowed for hard edges;
- endpoints outside the first/last stop clamp to the nearest stop;
- transforms operate in logical object or user space as selected by the paint;
- alpha interpolates with color;
- enough stop capacity for the eight-stop `Rating-S` gradient;
- a storage/uniform representation that does not require one shader module per
  gradient.

Before choosing `Rgba8Unorm` or `Rgba8UnormSrgb` as the canonical target,
render a small reference matrix containing:

- black-to-white midpoint;
- colored gradient midpoint;
- 50% alpha source-over;
- multiply composition;
- PNG texture sampling.

Compare it byte-for-byte with `resvg`. This establishes whether parity needs
raw sRGB-like byte math or hardware sRGB conversion. The decision is
engine-wide and must be documented in code; mixing policies between pipelines
will create seams.

Patterns required for current cards:

- 4x4 diagonal background tile;
- 5x5 dot pattern;
- repeated `hollows.png` with translation and -45-degree rotation.

The first two should be procedural to avoid tiny texture seams. The Hollows
pattern is an ordinary uploaded texture with repeat addressing and a paint
transform.

## 10. Persistent atlas and texture system

Every image draw references an `AtlasRegionHandle`. Scene nodes never contain
file paths, URLs, encoded PNG/WebP bytes, decoded CPU pixels, or a per-render
texture.

The renderer owns exactly one `ImageAtlasSet` instance for its lifetime. An
atlas set may contain several fixed-size GPU pages/texture-array layers, but
there is only one allocator, one key-to-region map, and one ownership domain.
Creating a second atlas set for a card or request is an error.

Atlas tiers:

- **Startup-static pages** contain bundled logos, stars, Hollows art, test
  fixtures, and assets listed by `StartupAssetManifest`. These regions are
  immutable, pinned, and never evicted.
- **Dynamic pages** belong to the same atlas set. The first capacity is
  reserved during startup. Assets whose URLs are not knowable until a request
  are inserted later. If the reserved pages fill, the atlas manager may append
  a page under its configured page/memory budget.
- **Oversize pages** are still owned by `ImageAtlasSet`. They are not
  free-standing card textures and are reused for compatible oversize assets.

It is impossible to upload an image that is not yet known at process startup,
such as a never-before-seen player's avatar. The enforceable invariant is:

1. the atlas set, allocator, layouts, samplers, and initial pages are created
   only at startup;
2. every statically knowable asset is uploaded before the renderer service is
   published;
3. a runtime asset is uploaded once into the existing atlas set during the
   resource-preparation stage;
4. resource preparation may append a page to the same atlas set when the
   configured budget permits;
5. no draw-command encoder/render pass creates a page, decodes an image, or
   uploads pixels;
6. repeated renders of a resident asset perform zero uploads.

Startup resource lifecycle:

1. Read the startup manifest.
2. Resolve bundled resources and configured preloaded disk-cache entries.
3. Decode independent PNG/JPEG/WebP/GIF inputs in parallel on a bounded CPU
   pool.
4. Normalize to RGBA8 and compute stable content hashes.
5. Deterministically sort by atlas class, dimensions, and stable key.
6. Pack regions and allocate the persistent GPU pages.
7. Upload the packed pages through `Queue::write_texture`.
8. Create persistent samplers and bind groups.
9. Drop temporary decoded CPU buffers after upload.
10. Publish the completed renderer.

Dynamic resource lifecycle:

1. Download outside the renderer.
2. Decode on the bounded resource pool.
3. Look up the content hash in the persistent atlas map.
4. Reserve a region in an existing dynamic page or append one budgeted page
   through the atlas manager.
5. Upload once and return an `AtlasRegionHandle`.
6. If the configured page/memory budget is exhausted, return a capacity error
   or run an explicit maintenance/repack policy; never allocate silently in a
   draw.

Required atlas behavior:

- key by stable asset identity and content hash;
- retain source dimensions, atlas page, UV rectangle, and format;
- deduplicate identical content reached through different URLs;
- keep startup-static regions pinned;
- use generation-safe handles if dynamic repacking is ever introduced;
- never move a region referenced by an in-flight render;
- expose lookup, hit, miss, upload-byte, page-occupancy, and capacity counters;
- invalidate an on-disk key when its content hash changes;
- use deterministic packing and deterministic local fixture assets in tests;
- prevent simultaneous requests from uploading the same key twice;
- maintain one GPU copy of each deduplicated image region.
- keep runtime-inserted regions resident for the renderer lifetime by default;
  if eviction is introduced, it must be explicit, generation-safe, and never
  duplicate a still-resident content hash.

Image fitting:

- `Contain`, `Cover`, `Fill`, and explicit UV rectangle are scene-level modes;
- current boss and avatar art use centered `Cover`;
- the UV calculation is tested for landscape, portrait, square, and fractional
  destination rectangles;
- filtering is explicitly linear;
- address mode is clamp for ordinary art and repeat only for pattern paints.

An encoder assertion must fail a test if any `ImageNode` reaches the GPU
without a resident atlas region. A second assertion records zero page
creation and zero upload bytes across an ordinary warm render. Runtime tests
assert that first use performs one insertion and subsequent/concurrent use
performs none.

## 11. Clipping, masks, and blending

### 11.1 Basic blend modes

Required before card migration:

- `SourceOver` for ordinary layers;
- `Multiply` for the legacy art-mask composition;
- `Replace` for clears and mask initialization.

`Add` and `Screen` may be defined in the enum but are not parity requirements
until tested and implemented.

All custom pipelines must use the same alpha convention. The initial plan is
straight-color shader output with explicit source-over factors, but the color
characterization test in section 9 has authority over the final choice.

### 11.2 Clips

- Axis-aligned rectangular clips use scissor rectangles.
- Rounded-rectangle and circle clips use an alpha mask or stencil-like
  bounded intermediate.
- Nested clips intersect.
- Boss art is clipped to the inner rounded card contour.
- Avatar textures are clipped to their 24-unit circle.
- Fractional edges are antialiased; a binary scissor is insufficient for
  rounded clips.

### 11.3 Masks

Represent masks explicitly as alpha values:

- `Alpha`: content alpha multiplied by mask alpha;
- `Multiply`: mask layers multiply, matching the existing horizontal and
  vertical art fades;
- optional `InvertAlpha` can support future assets without changing the graph.

Boss-art rendering order:

1. compute centered cover UV;
2. draw image into the boss-art bounds;
3. multiply by horizontal fade;
4. intersect with rounded-card clip;
5. source-over onto the card;
6. draw the complex-boss burgundy stroke above the art.

Transient mask and color targets are pooled by physical width, height, format,
and usage. Clear pooled targets before reuse.

### 11.4 Extensible layer effects

Effects are ordered operations on a bounded input image. They are not encoded
as special cases in individual card builders.

Initial typed API:

```rust
pub enum Effect {
    Fill(Paint),
    AlphaMask(MaskSource),
    Opacity(f32),
    ColorMatrix(ColorMatrix),
    Blur(GaussianBlur),
    Builtin(BuiltinEffectId, EffectParameters),
    Custom(CustomEffectId, EffectParameters),
}

pub trait EffectCompiler: Send + Sync {
    fn validate(&self, parameters: &EffectParameters) -> Result<()>;
    fn expand_logical_bounds(
        &self,
        input: LogicalRect,
        parameters: &EffectParameters,
    ) -> Result<LogicalRect>;
    fn compile(
        &self,
        context: &EffectCompileContext,
        input: GraphTexture,
        parameters: &EffectParameters,
    ) -> Result<GraphTexture>;
}
```

The concrete trait/API can change during implementation, but the separation
must remain:

- scene nodes store effect intent and serializable/owned parameters;
- `EffectRegistry` maps stable IDs to compilers and immutable pipelines;
- compilers append render/compute passes to the graph;
- an effect declares bound expansion, input/output formats, scale behavior,
  temporary-resource needs, and whether it preserves alpha;
- card builders never allocate a target or encode a pass;
- unsupported custom IDs fail during scene validation.

Required first-class effects:

- `Fill`: replace/colorize RGB through a `Paint` while preserving input alpha;
- `AlphaMask`: multiply input alpha by another layer/texture/analytic mask;
- `Opacity`;
- `ColorMatrix` as the general foundation for tint/saturation adjustments;
- separable Gaussian `Blur`;
- text-mask expansion as an internal effect using the same graph extension
  mechanism.

Effect-chain semantics:

1. Render the layer subtree into its minimal expanded bounds.
2. Run `effects[0]` on that result.
3. Feed its output to `effects[1]`, and so on.
4. Apply layer opacity and blend the final result into the parent.

Identity effects must be removable by graph optimization only after validation.
Effects with nonlocal sampling must report the required logical padding so
blur/shadow pixels are not clipped.

### 11.5 Backdrop effects

A backdrop effect filters destination content already painted behind a layer.
It is a different dependency from a layer-local effect and therefore has a
separate type:

```rust
pub enum BackdropEffect {
    Blur(GaussianBlur),
    ColorMatrix(ColorMatrix),
    Custom(CustomBackdropEffectId, EffectParameters),
}
```

Backdrop compilation for a clipped layer:

1. Compute effect bounds from the layer bounds, clip, and filter padding.
2. Copy the affected region of the current parent target to a transient
   snapshot texture. Sampling from the active render attachment is forbidden.
3. Run backdrop effects in declaration order on the snapshot.
4. Composite the filtered snapshot back through the layer clip/mask.
5. Draw the layer's own content above it according to declared semantics.
6. Continue painting later siblings.

For `BackdropBlur`:

- use separable horizontal and vertical passes;
- express sigma/radius in logical units and scale it for physical kernels;
- cap radius through validated renderer limits;
- expand capture bounds by the documented kernel support;
- define edge sampling explicitly (initially clamp-to-edge);
- pool ping/pong textures;
- avoid full-canvas copies when the clipped bounds are smaller;
- allow a future compute implementation without changing the scene API.

Backdrop correctness tests:

- an impulse/kernel micro-scene;
- constant-color preservation;
- left/right/top/bottom edge behavior;
- clip isolation;
- it sees earlier siblings but never later siblings;
- blur radius scales with zoom without changing logical coverage;
- two backdrop effects observe the expected sequential destination state;
- zero radius is an identity and creates no filter pass;
- failure bundle includes the pre-effect snapshot when a layer-capture test
  diverges.

## 12. Text with glyphon

Use glyphon as the only glyph shaping/rasterization/rendering path.

Persistent renderer-owned objects:

- `FontSystem`;
- `SwashCache`;
- glyphon `Cache`;
- `Viewport`;
- `TextAtlas`;
- one or more `TextRenderer` instances for compatible pipelines;
- shaped-buffer cache keyed by text, spans, metrics, bounds, and font
  generation.

There is one of each persistent glyphon object per renderer service. A card
must never create its own `FontSystem`, `TextAtlas`, or `TextRenderer`.

Initialization:

1. Create the wgpu device and queue.
2. Create glyphon `Cache`, `Viewport`, and `TextAtlas` with the canonical
   target format.
3. Load `inpin.ttf` bytes explicitly into `FontSystem`.
4. Resolve and record the embedded family name.
5. Map every card style to that explicit family. Do not depend on host system
   fonts. The current `Mont` request must receive an intentional mapping.
6. Create `TextRenderer` with single-sample state and no depth buffer.
7. Shape and prepare a startup glyph manifest containing all static card
   labels, digits, punctuation, watermark text, and configured language
   alphabets at every used font-size bucket.
8. Submit the warmup so glyphs are resident before the renderer service is
   published.
9. Record atlas page count/bytes and reset upload counters for warm-render
   assertions.

An arbitrary nickname or future localized string can contain a glyph that is
not knowable at startup. Such a glyph is inserted into the same persistent
glyphon atlas once during text preparation, never by constructing another
atlas. Concurrent requests for the same missing glyph must coalesce through
the single renderer owner. Static/prewarmed glyphs are pinned for the process
lifetime.

For each frame:

1. Update the viewport to the physical target resolution.
2. Build/reuse a `Buffer` with logical font metrics and logical wrap bounds.
3. Apply styled spans through cosmic-text attributes.
4. Shape with advanced shaping.
5. Compute anchor and baseline offsets from the shaped layout.
6. Call `prepare` or `prepare_with_depth` with `TextArea::scale` equal to the
   render scale.
7. Render into either the card target or a bounded text-mask target.
8. Do not call `trim` for startup-pinned glyphs. Any dynamic-glyph retention
   policy must preserve the single atlas instance and expose eviction metrics.

Warm-render tests assert that a card containing only startup-manifest text
does not grow the glyph atlas or upload glyph pixels.

### 12.1 Anchors and baselines

Implement and test:

- horizontal start, center, and end anchors;
- top, alphabetic baseline, middle/dominant-middle compatibility;
- explicit line height;
- `dx`, `dy`, and per-line x reset used by existing templates.

Do not reproduce SVG baseline behavior through unexplained magic constants.
Store compatibility offsets in named card text styles and cover them with
fixture tests.

### 12.2 Inline spans and wrapping

Keep the existing game-text cleanup and wrapping behavior as shared data
preparation:

- `<Term>` and `<IconMap>` removal;
- prohibited punctuation at line start;
- current logical-width wrapping;
- color spans closed and reopened across line boundaries.

Replace generated `<tspan>` markup with a typed sequence:

```rust
pub struct TextSpan {
    pub range: Range<usize>,
    pub paint: TextPaint,
}
```

The same shaped buffer must establish positions for all spans so style changes
cannot alter wrapping.

### 12.3 Gradient fill and text outline

Glyphon renders glyph coverage. Existing cards also require gradient text fill
and thin outlined text, so direct solid-color drawing is not sufficient.

Planned path:

1. Shape the complete text once.
2. For each distinct text paint, render matching glyphs in white and all other
   glyphs transparent into a bounded RGBA mask target using glyphon.
3. Sample the mask alpha in the composite shader.
4. Fill it with solid/linear/radial `Paint`.
5. For an outline, expand the alpha mask by the logical outline radius scaled
   to physical pixels.
6. Subtract/occlude the inner fill coverage where required.
7. Composite outline first and fill second, matching SVG `paint-order`.

The mask-expansion spike must compare multi-offset, separable maximum filter,
and distance-based methods at widths `0.23, 0.25, 0.28, 0.3, 0.33, 0.36,
0.4, 0.45, 0.5, 1.0, 1.25` logical units. Select the smallest method that
matches the reference at zoom 1.0 and remains stable at zoom 5.0.

Text parity is a high-risk milestone and must be completed before converting
all seven card builders.

## 13. Render graph and command encoding

Build a compact per-frame render graph from the flattened display list.

Pass kinds:

- main color pass;
- glyph mask pass;
- generic alpha-mask pass;
- text-outline expansion pass;
- layer-local effect pass;
- backdrop region snapshot;
- horizontal/vertical backdrop blur;
- composite pass;
- texture-to-buffer copy.

Graph construction rules:

- allocate intermediates only for bounded affected regions;
- reuse pooled textures only when the previous submission is complete;
- preserve painter's order across passes;
- represent effect inputs/outputs as explicit graph edges;
- reject cycles, including a backdrop pass that attempts to sample its own
  destination attachment;
- run independent CPU preparation nodes concurrently and submit their GPU
  uploads in deterministic atlas order;
- insert no CPU readback between drawing passes;
- keep one command encoder per card unless backend limits require otherwise;
- label textures, passes, pipelines, buffers, and command encoders for GPU
  diagnostics;
- wrap command encoding in wgpu error scopes during development and tests.

The initial implementation should prefer correctness and transparent pass
boundaries. Batch and pass fusion only after pixel parity.

## 14. Headless target, readback, and PNG

Render to an offscreen 2D texture; no `Surface` or `winit` dependency is
needed.

Target usage:

- `RENDER_ATTACHMENT`;
- `COPY_SRC`;
- `TEXTURE_BINDING` when a later composite samples the target.

Readback sequence:

1. Compute unpadded row bytes as `physical_width * 4`.
2. Round row bytes up to wgpu's 256-byte copy alignment.
3. Allocate/reuse a `COPY_DST | MAP_READ` staging buffer.
4. Encode `copy_texture_to_buffer`.
5. Submit once.
6. Map asynchronously and wait through the supported wgpu polling API.
7. Copy each unpadded row into contiguous RGBA8.
8. Swizzle BGRA to RGBA if the selected target requires it.
9. Unmap promptly.
10. Encode PNG on a bounded CPU worker.

The test engine already exercises row-unpadding and BGRA conversion without a
GPU dependency.

## 15. Renderer service integration

The current bot wraps CPU rendering in `spawn_blocking`. The GPU renderer
should instead be initialized once and called through a bounded async service.

Integration steps:

1. Add render configuration to `BotState`.
2. Build the startup asset/glyph manifest.
3. Initialize the renderer, persistent atlases, and known atlas contents; log
   adapter/backend/limits/page occupancy.
4. Start a renderer-owner task with a bounded request queue only after startup
   warmup succeeds.
5. Change handlers and scheduler to prepare data and resources asynchronously.
6. Coalesce and insert newly encountered runtime images before scene
   submission.
7. Send typed card requests with explicit `RenderScale`.
8. Await PNG response.
9. Preserve the existing per-chat error isolation.
10. Add request timeout and cancellation behavior.
11. Keep the reference backend selectable during rollout.

Suggested rollout configuration:

```text
BOT_RENDER_BACKEND=resvg|gpu|compare
BOT_RENDER_ZOOM=5.0
BOT_RENDER_QUEUE_CAPACITY=<bounded>
BOT_RENDER_TEXTURE_CACHE_BYTES=<bounded>
```

`compare` renders both backends at 1.0 for fixtures/development. It must not
double-render ordinary production Telegram requests unless explicitly enabled.

## 16. Test engine

The initial test engine lives in:

- `crates/rustverse_svg/tests/support/render_compare.rs`
- `crates/rustverse_svg/tests/render_test_engine.rs`

Implemented foundation:

- validated RGBA8 image buffer;
- PNG decode and normalization to RGBA8;
- PNG encode;
- 256-byte GPU row-alignment calculation;
- padded RGBA/BGRA readback normalization;
- exact or explicitly thresholded pixel comparison;
- per-channel counts and maxima;
- absolute and RMS metrics;
- diff bounding box;
- failure bundle writer;
- hard requirement that reference comparison uses exactly scale 1.0.

Next test-engine work:

- helper that runs reference and GPU closures for the same fixture;
- standard failure directory under the selected Cargo target directory;
- adapter/backend metadata in `report.txt`;
- scene display-list dump beside image diffs;
- optional layer-by-layer captures to find the first divergent layer;
- golden update command requiring explicit `UPDATE_RENDER_GOLDENS=1`;
- manifest containing fixture name, logical size, scale, renderer versions,
  font hash, image hashes, and approved comparison policy;
- CI artifact upload for every failed comparison;
- deterministic sort order for batch fixture execution.

## 17. Test pyramid

### 17.1 Pure scale tests

- Reject zero, negative, NaN, and infinities.
- Accept representative fractional and integer scales.
- Verify rounded physical dimensions.
- Verify max texture and byte-budget rejection.
- Verify scissor floor/ceil behavior.
- Verify that changing scale does not change scene structure or text wrapping.

### 17.2 Test-engine self-tests

- Exact match gives no diff.
- Multiple changed channels produce correct counts and maxima.
- Bounding box spans all changed pixels.
- Channel threshold is applied before the differing-pixel budget.
- A non-1.0 parity comparison is rejected.
- GPU row padding is removed.
- BGRA is converted to RGBA.
- PNG round trip preserves bytes.
- Failure bundles contain all expected artifacts.
- Dimension mismatch fails before metrics are computed.

### 17.3 Shader micro-scenes

At zoom 1.0, compare tiny deterministic scenes for:

- solid clear and solid rectangle;
- subpixel rectangle edges;
- rounded rectangle fill;
- rounded rectangle stroke;
- circle fill and stroke;
- source-over alpha;
- multiply;
- every shared linear gradient;
- radial gradient;
- duplicate gradient stops;
- dot and diagonal patterns;
- transformed repeated texture;
- rectangular, circular, and rounded clips;
- nested clip intersection;
- horizontal and vertical mask fades.

Micro-scenes should be small enough that a single pixel can be reasoned about
from the fixture.

### 17.4 Texture tests

- Upload and draw RGBA PNG.
- Upload and draw decoded JPEG/WebP.
- Centered `Cover` crop for portrait, landscape, and square sources.
- Linear filtering at fractional coordinates.
- Clamp versus repeat addressing.
- Cache deduplication.
- Startup manifest packing is deterministic.
- Renderer startup creates exactly one image atlas set.
- A first runtime image insertion appends/uploads at most once.
- Concurrent first-use requests coalesce to one insertion.
- A warm render creates no atlas page and uploads zero bytes.
- Explicit cache eviction, if enabled, does not affect in-flight renders.
- Missing/nonresident atlas region is rejected.
- All card scene image handles resolve before encoding.

### 17.5 Text tests

- Embedded font is selected on a machine with no useful system fonts.
- ASCII, punctuation, CJK, and mixed text.
- Start, center, and end anchors.
- Dominant-middle and baseline placement.
- Single and multiline buffers.
- Explicit line height.
- Existing wrap behavior and colored spans across lines.
- Default vertical gradient fill.
- Solid inline color span.
- Thin gradient outline.
- Rank/score font sizes from 6.2 through 25.
- Watermark style.
- Scale matrix without reflow.
- Repeated renders reuse glyph atlas entries.
- Renderer startup creates exactly one font/glyph atlas set.
- Prewarmed text causes zero glyph upload on the first served render.
- An unknown runtime glyph grows the existing atlas rather than creating one.

### 17.6 Component scenes

Create fixtures for:

- shared background;
- leaderboard row, including first-place gradient;
- player room with three avatars, stars, boss art, and buff icon;
- Deadly buff card;
- normal Deadly boss card;
- complex Deadly boss card;
- Shiyu boss card with secondary enemies;
- room with no elements and compact mechanics offset;
- watermark.

### 17.7 Full-card fixtures

Each of the seven public card variants needs at least:

- minimal/empty legal data;
- ordinary representative data;
- maximum text/layout stress data;
- local deterministic art only;
- scale 1.0 reference comparison;
- scale matrix dimension/layout tests.

Deadly season fixtures must include normal and complex modes. Shiyu fixtures
must include stage 5 child-zone sorting, resistance, several mechanics lines,
and the highest-HP featured boss.

### 17.8 Integration and operational tests

- Concurrent requests serialize safely through the renderer service.
- Queue capacity enforces backpressure.
- A cancelled caller does not poison renderer state.
- One failed image does not fail later requests.
- Device-lost path recreates state or returns the documented fatal error.
- PNG output starts with the PNG signature and has expected dimensions.
- Handlers and scheduler no longer use `spawn_blocking` for GPU drawing.

### 17.9 Benchmarks

Measure separately:

- scene construction;
- image decode;
- first texture upload;
- warm texture-cache render;
- glyphon prepare;
- GPU submission to map completion;
- row unpadding;
- PNG encode;
- end-to-end cold and warm render;
- throughput under the expected queue depth;
- peak GPU texture bytes and staging bytes.

Benchmark all three major card shapes: `640x360`, fixed-width leaderboard, and
tall dynamic season information card, at scales 1.0 and 5.0.

## 18. Fixture policy

- Never use live network images in parity tests.
- Store small pinned local fixtures with recorded hashes.
- Reuse bundled production art when it exercises the needed path.
- Do not commit generated ad-hoc renders from manual development.
- Commit only intentional reference goldens and fixture assets.
- Generate reference goldens through the scale-aware compatibility renderer at
  exactly 1.0.
- Record the `resvg`, glyphon, wgpu, font, and fixture versions in the golden
  manifest.
- A golden update must be a reviewable, explicit action.
- Dependency upgrades affecting rasterization require the full pixel suite.

## 19. Parallel execution DAG

Top-level stages are sequential because each ends in a contract/merge gate.
Work packages inside a stage are deliberately independent and can be assigned
to different implementers. A package may merge behind an internal feature
flag once its own tests pass; the next stage starts only when the shared gate
is green.

```text
S0 Baseline
  A0 reference scale + goldens
  B0 pixel test engine
  C0 feature/color inventory
          |
          v G0: reference contract frozen

S1 Foundations (parallel)
  A1 model extraction + scene API --------------------+
  B1 headless wgpu/readback --------------------------+
  C1 startup manifest + atlas allocator design -------+--> G1
  D1 effect/backdrop API + render-graph IR -----------+
  E1 fixture/CI artifact plumbing --------------------+

S2 Core rendering (parallel after G1)
  A2 analytic shapes + gradients + patterns ----------+
  B2 image atlas packing/upload/sampling --------------+
  C2 glyphon singleton atlas + startup prewarm --------+--> G2
  D2 transient target pool + graph scheduler ----------+
  E2 runtime asset coalescing/capacity ----------------+

S3 Composition and effects (parallel after G2)
  A3 clips + masks + source-over/multiply -------------+
  B3 fill/opacity/color-matrix/local blur effects -----+
  C3 backdrop capture + backdrop blur -----------------+--> G3
  D3 glyph gradient fill + outline effects ------------+
  E3 scale/failure/adapter stress suites ---------------+

S4 Card migration (parallel after G3)
  A4 shared background + leaderboard cards ------------+
  B4 player DA/Shiyu cards -----------------------------+--> G4
  C4 Deadly/Shiyu season-info cards --------------------+
  D4 renderer-service integration (can begin at G2) ----+

S5 Rollout/hardening (parallel after G4)
  A5 bot handler/scheduler cutover ---------------------+
  B5 benchmarks/cache tuning ---------------------------+--> G5
  C5 device-loss/limits/CI backend ---------------------+
  D5 full pixel/visual review --------------------------+

S6 Cleanup
  remove SVG backend and optional crate rename
```

### G0 — Reference contract gate

Required:

- current feature inventory accepted;
- scale 1.0 reference generation works;
- scale 5.0 remains the default;
- pixel test engine and failure bundle work;
- deterministic fixtures are selected.

### G1 — Foundation API gate

Required:

- scene/effect/atlas public types compile together;
- headless clear/readback works;
- atlas set and effect registry have exactly one renderer owner;
- scale and physical-size policy are shared by every subsystem;
- no work package has introduced a second copy of preparation models.

### G2 — Core rendering gate

Required:

- shapes, gradients, patterns, image-atlas sampling, and direct glyphon text
  can render in one graph;
- known assets/glyphs are startup-resident;
- runtime image insertion is deduplicated and occurs before encoding;
- warm render shows zero atlas construction and zero known-resource uploads;
- transient targets are reusable and submission-safe.

### G3 — Composition/effect gate

Required:

- clips, alpha masks, fill, source-over, and multiply pass pixel micro-scenes;
- local effect chains preserve order;
- backdrop capture observes only prior destination content;
- backdrop blur passes kernel, clip, edge, and scale tests;
- gradient/outlined glyphon text meets reviewed pixel policy;
- custom/unknown effect validation fails cleanly.

### G4 — Full-card parity gate

Required:

- all seven card variants have GPU scene builders;
- card families can be developed/reviewed independently but share the same
  background, paint, text, atlas, and effect implementations;
- all full-card scale 1.0 pixel comparisons and scale matrix tests pass.

### G5 — Production gate

Required:

- renderer service is bounded and observable;
- production startup fully warms manifest assets/glyphs;
- runtime atlas growth respects capacity and coalescing;
- cold/warm benchmarks and GPU memory are recorded;
- canary error/latency targets are met;
- reference fallback has a dated removal decision.

### Work package boundaries

To keep parallel work mergeable:

- one package owns each public type until G1; other packages consume it rather
  than creating competing types;
- WGSL files are split by pipeline/effect and share only a reviewed common
  color/transform include;
- card packages may edit separate builder/test files after shared components
  are frozen at G3;
- atlas/resource code owns uploads; card and effect code receives handles;
- render-graph code owns transient targets; effect compilers request graph
  resources rather than allocating directly;
- test-engine code owns diff formats and failure output; feature packages add
  fixtures/policies without forking comparison logic;
- every parallel package includes tests and a short compatibility note before
  its merge gate.

## 20. Migration phases

Each phase ends at its exit gate. Do not begin mass card conversion while a
lower-level gate is red.

### Phase 0 — Baseline and contract

Deliverables:

- complete feature inventory;
- scale contract;
- scale-aware `resvg` reference function;
- pixel-comparison test engine;
- representative CPU reference fixtures;
- color-space characterization scene specification.

Tests:

- scale validation and dimension matrix;
- test-engine self-tests;
- current `rustverse_svg` release tests.

Exit gate:

- existing public output still defaults to scale 5.0;
- reference output can be generated at 1.0;
- comparator artifacts are actionable.

Current status: complete. The scale-aware reference API preserves the 5.0
compatibility default, the comparator and diagnostic bundle are covered by
self-tests, and `top_da_single_entry` is the first pinned scale-1.0 CPU
reference fixture. Its manifest records exact comparison policy, renderer
versions, logical dimensions, and SHA-256 hashes for the input, golden,
templates, font, and bundled images. Gate G0 is closed.

### Phase 1 — Module extraction without visual changes

Deliverables:

- split current view preparation from SVG backend;
- move template/resvg code under `reference_svg`;
- introduce `scene`, `cards`, and `renderer` module shells;
- preserve all public functions.

Tests:

- full current release suite;
- byte-identical PNG output for pinned reference fixtures before and after the
  refactor.

Exit gate:

- no data-preparation model is duplicated;
- reference renderer output is unchanged.

Current status: complete. The crate root is now a compatibility facade;
prepared view models and layout logic live in `model`, MiniJinja/resvg and
legacy asset loading live in the private `reference_svg` module, and
backend-neutral `scene`, `cards`, and `renderer` contracts are present. Public
entry-point signatures are compile-time tested and the pinned scale-1.0
reference PNG remains byte-identical.

### Phase 2 — Headless GPU skeleton

Deliverables:

- pinned compatible `wgpu` and `glyphon` versions;
- adapter/device/queue initialization without a surface;
- validated `RenderScale`;
- offscreen target creation;
- clear pass;
- aligned readback;
- PNG output;
- singleton `ImageAtlasSet`, singleton glyphon state, and `EffectRegistry`
  ownership shells;
- renderer service prototype.

Tests:

- adapter initialization;
- clear-color micro-scenes;
- RGBA/BGRA readback;
- scale physical sizes;
- repeated renderer reuse.
- startup constructs each atlas set exactly once.

Exit gate:

- a warm renderer emits deterministic solid PNGs at every scale in the matrix;
- no device is created per render.

Current status: complete. `wgpu` 30.0.0 and `glyphon` 0.12.0 are pinned to
one compatible dependency graph. The surface-free renderer owns one
adapter/device/queue and one persistent image-atlas, glyphon-state, and effect
registry aggregate; the integration gate is verified on an Apple M2 Metal
adapter. Its bounded single-owner service reuses that state while clear passes
render deterministic RGBA8 PNGs at scales 0.5, 1.0, 1.25, 2.0, and 5.0.
The owner loop runs on a dedicated thread, and target/staging byte budgets plus
device limits are validated before GPU allocation. Aligned readback, request
error isolation, queued cancellation, and queue backpressure are covered by
release tests.

### Phase 3 — Shapes, paints, gradients, and patterns

Deliverables:

- instanced shape pipeline;
- solid, linear-gradient, and radial-gradient paint;
- rounded fill/stroke;
- procedural dot and diagonal patterns;
- transformed repeated-texture pattern;
- color-space decision.

Tests:

- all primitive and gradient micro-scenes;
- exact pixel gate at 1.0 where achievable;
- scale invariants.

Exit gate:

- shared background and empty card shells render without `resvg`.

### Phase 4 — Persistent atlas resources and image fitting

Deliverables:

- image decoder boundary;
- deterministic startup manifest/packing;
- persistent atlas-page allocation/upload;
- atlas bind groups and samplers;
- centered `Cover`;
- bundled and cached remote resource adapters;
- runtime asset insertion/coalescing;
- page/memory metrics and budget.

Tests:

- texture test matrix;
- every image format currently accepted;
- resident-region validation;
- singleton/startup preload tests;
- first runtime insertion and concurrent deduplication;
- warm zero-upload tests;
- atlas lifetime/capacity tests.

Exit gate:

- logos, stars, Hollows art, bosses, agents, and buff icons all render from
  persistent GPU atlas regions;
- encoded draw commands contain no file/URL/encoded-image source.

### Phase 5 — Clips, masks, blending, effects, and backdrop

Deliverables:

- scissor clip;
- rounded/circular alpha clip;
- bounded mask passes;
- source-over and multiply;
- transient-target pool;
- boss-art fade composition;
- effect registry/compiler contract;
- fill, opacity, alpha-mask, color-matrix, and local blur effects;
- bounded backdrop snapshot;
- separable backdrop blur;
- custom effect validation path.

Tests:

- clip/mask/blend micro-scenes;
- effect-chain order and bounds-expansion scenes;
- backdrop dependency, kernel, edge, clip, and scale scenes;
- normal and complex boss-art components;
- nested clips at fractional scales.

Exit gate:

- boss and avatar art never crosses its contour;
- complex Deadly border renders above clipped art;
- no stale pooled-mask/effect content appears;
- backdrop effects cannot sample the active attachment or later siblings;
- a new registered effect can append passes without changing card builders.

### Phase 6 — Glyphon text parity

Deliverables:

- persistent glyphon state;
- embedded-font loading;
- startup glyph manifest and atlas prewarm;
- typed text styles/spans;
- anchors, baselines, line heights;
- gradient text fill;
- text-outline path;
- shaped-buffer and atlas caching.

Tests:

- full text matrix;
- scale-without-reflow tests;
- singleton glyph atlas, startup residency, runtime glyph growth, and reuse;
- component pixel diffs.

Exit gate:

- all shared text styles, including watermark and inline colors, meet their
  reviewed pixel policies at 1.0;
- known glyphs produce zero warm-render atlas uploads;
- no alternative text rasterizer is used.

### Phase 7 — Parallel card-family migration

Deliverables:

- shared background builder frozen first;
- parallel leaderboard work package: row builder, `top_da`, `top_shiyu`, and
  first-place gradient treatment;
- parallel player-result work package described in Phase 8;
- parallel season-information work package described in Phase 9;
- public backend selection.

Tests:

- minimal, normal, long-name, and long-list leaderboard fixtures;
- scale matrix;
- full-card pixel comparison.

Exit gate:

- both leaderboard entry points can use the GPU backend in compare mode;
- shared component/API changes needed by Phases 8/9 are resolved before their
  merge.

### Phase 8 — Player-result card work package

Deliverables:

- player room component;
- avatar rings and clips;
- stars, score/rating, and buff icon;
- `da` and `shiyu` scene builders.

Tests:

- zero/one/two/three-star cases;
- ranks, three avatars, missing optional art where legal;
- score/rating variants;
- full-card pixel comparison and scale matrix.

Exit gate:

- both player-result entry points meet parity and resource-residency gates.

### Phase 9 — Season-information card work package

Deliverables:

- Deadly buff and boss components;
- complex boss variant;
- Shiyu room component;
- dynamic-height scene builders;
- `deadly_info`, `deadly_info_with_begin_time`, and `shiyu_info`.

Tests:

- current season fixtures;
- complex-mode fixture;
- no-elements compact offset;
- very long mechanics and buffs;
- multiple Shiyu secondary enemies;
- full-card pixel comparison and scale matrix.

Exit gate:

- all dynamic cards have correct heights with no overlap or clipping;
- all seven public card variants meet parity.

### Phase 10 — Bot rollout

Deliverables:

- renderer service in `BotState`;
- backend and scale configuration;
- handler/scheduler integration;
- structured render metrics;
- startup adapter/atlas/prewarm diagnostics;
- runtime asset-insertion path and capacity handling;
- compare/canary mode.

Tests:

- bot handler and scheduler suites;
- concurrency, cancellation, queue, and failure isolation;
- startup-resident and runtime-added image integration;
- manual Telegram media validation.

Exit gate:

- GPU backend is the default in a canary environment;
- error and latency metrics are acceptable;
- default scale 5.0 output respects Telegram limits.

### Phase 11 — Performance and hardening

Deliverables:

- benchmark suite;
- static/dynamic atlas sizing;
- transient-target reuse;
- effect/backdrop pass and ping/pong tuning;
- conservative consecutive batching;
- device-loss policy;
- allocation and input limits;
- CI GPU/software-adapter strategy.

Tests:

- cold/warm benchmark records;
- stress queue;
- repeated tall-card renders;
- runtime atlas growth/capacity and optional generation-safe eviction;
- repeated backdrop-blur scenes;
- device error injection where possible.

Exit gate:

- measured GPU path materially reduces rasterization bottleneck;
- no unbounded cache or queue;
- no per-render device/pipeline/atlas creation and no repeated resident-asset
  upload.

### Phase 12 — Remove SVG backend

Deliverables:

- remove MiniJinja graphics templates;
- remove `resvg`, `usvg`, and `tiny-skia` production dependencies;
- remove compatibility backend selection;
- retain approved goldens and comparator;
- optionally rename `rustverse_svg` to `rustverse_render`.

Tests:

- release workspace suite;
- GPU pixel suite;
- all bot integration tests;
- representative manual renders at scale 1.0 and 5.0.

Exit gate:

- no production SVG string is generated or parsed;
- every image draw uses a texture;
- every text draw uses glyphon;
- all public render behaviors and scheduler invariants remain intact.

## 21. Dependency and version policy

Glyphon and wgpu evolve together. Pin compatible versions exactly during the
parity work rather than using broad semver ranges. Record the pair in the
golden manifest.

The current upstream API shape uses:

- glyphon `Cache`;
- `Viewport`;
- `TextAtlas`;
- `TextRenderer::prepare` / `prepare_with_depth`;
- `TextRenderer::render`;
- cosmic-text types re-exported by glyphon;
- a wgpu render pass supplied by this engine.

Do not design a wrapper around obsolete glyphon examples. Complete the Phase
2 spike against the pinned crate source and compile on all deployment targets
before deeper implementation.

Suggested additional dependencies must earn their place:

- `wgpu` and `glyphon`: required;
- image decoders: use narrow crates/features for PNG/JPEG/WebP/GIF;
- `bytemuck`: instance/uniform serialization;
- `pollster`: tests/examples only if the production service remains async;
- path tessellation crate: defer until a real scene requires arbitrary paths.

## 22. Error model and limits

Define renderer errors with actionable categories:

- invalid scale;
- invalid/non-finite scene geometry;
- physical dimension overflow;
- texture dimension/byte budget exceeded;
- unresolved asset;
- decode failure;
- atlas page/region capacity exceeded;
- atlas upload failure;
- unsupported/invalid effect;
- effect-bound expansion overflow;
- cyclic/invalid backdrop dependency;
- glyphon prepare/render failure;
- wgpu validation or out-of-memory error;
- device lost;
- buffer-map/readback failure;
- PNG encode failure.

Limits:

- maximum physical width/height from device limits and configuration;
- maximum physical pixels and RGBA bytes per card;
- maximum gradient stops;
- maximum scene nodes;
- maximum nested clip/mask depth;
- maximum effects/backdrop effects per layer;
- maximum blur radius and expanded effect pixels;
- maximum decoded texture dimensions/bytes;
- bounded static/dynamic image-atlas pages;
- bounded transient pool;
- bounded render queue;
- bounded shaped-text cache.

Reject invalid work before GPU allocation whenever possible.

## 23. Observability

Per-render structured fields:

- card kind;
- logical and physical dimensions;
- zoom factor;
- scene-node and display-command counts;
- unique textures and uploaded bytes;
- static/dynamic image-atlas page count, occupancy, new runtime regions, and
  coalesced insertions;
- glyph-atlas page/byte growth and newly rasterized glyph count;
- texture/glyph cache hits and misses;
- number of render/composite/mask/effect/backdrop passes and backdrop copied
  pixels;
- scene build, resource prepare, glyphon prepare, submit/readback, and PNG
  durations;
- adapter/backend;
- final PNG byte count;
- error category.

Never log URLs containing secrets, cookies, raw private result payloads, or
Telegram identifiers beyond the project's existing safe logging policy.

## 24. Main risks and decision spikes

### Text rasterization difference

Glyphon/cosmic-text/Swash and resvg may place or antialias the same font
differently. Mitigation: complete text micro-scenes early, pin font bytes and
versions, separate layout from edge-raster differences, and keep per-fixture
pixel metrics visible.

### Gradient and blend color space

Hardware sRGB conversion may differ from SVG/tiny-skia behavior. Mitigation:
the Phase 3 characterization matrix decides the canonical format before card
work.

### Text outlines

Glyphon has no direct SVG-style path stroke contract. Mitigation: bounded mask
expansion spike across every current logical stroke width before migration.

### GPU availability in deployment and CI

Headless hosts may expose different backends or only software adapters.
Mitigation: startup diagnostics, explicit adapter policy, a required parity
lane on the deployment-class backend, and an intentional rollout fallback
until cutover.

### Readback and PNG remain CPU costs

GPU rendering still needs transfer and PNG compression. Mitigation: benchmark
the stages separately, reuse staging buffers, keep one submission, and encode
on bounded workers.

### Excessive offscreen passes

Masks and gradient text can create many targets. Mitigation: bound each target,
pool it, group compatible text masks without changing order, and optimize only
after parity measurements.

### Runtime atlas growth

Remote boss/agent art changes over time. Mitigation: content-aware keys,
single-flight insertion, startup-reserved dynamic pages, hard page/byte
budgets, optional generation-safe eviction, in-flight pinning, and metrics.

### Backdrop dependency and cost

Backdrop effects depend on already-painted destination content and can force
copies/pass breaks. Mitigation: explicit graph edges, bounded capture regions,
cycle validation, separable blur, pooled ping/pong targets, and pass-count/
copied-pixel metrics.

## 25. Definition of done

- [ ] All seven public card variants have typed scene builders.
- [ ] Production rendering creates no SVG, XML, CSS, or MiniJinja graphics.
- [ ] `resvg` and related production dependencies are removed.
- [ ] All text is shaped/rasterized/rendered through glyphon.
- [ ] The embedded font is selected deterministically.
- [ ] Every image is decoded and resident in a GPU texture before drawing.
- [ ] One persistent image atlas set is created at startup.
- [ ] Startup-manifest images are preloaded before requests are accepted.
- [ ] Runtime images are single-flight inserted once into the existing atlas
      set and reused without repeat upload.
- [ ] One persistent glyphon atlas set is created and prewarmed at startup.
- [ ] Runtime glyphs grow the existing glyph atlas without creating another.
- [ ] Solid, linear, and radial paint work.
- [ ] Rounded shapes, circles, fills, and strokes work.
- [ ] Patterns match the current backgrounds.
- [ ] Source-over and multiply composition work.
- [ ] Ordered layer-local fill/mask/opacity/color/blur effect chains work.
- [ ] New effects can be registered without editing card builders.
- [ ] Bounded backdrop capture and backdrop blur work.
- [ ] Backdrop effects see prior layers only and cannot create graph cycles.
- [ ] Rectangular, circular, rounded, and nested clipping work.
- [ ] Alpha masks and boss-art fades work.
- [ ] Gradient text and outlined text work.
- [ ] Layer ordering is stable and testable.
- [ ] Render scale is supplied per render.
- [ ] Default scale remains 5.0.
- [ ] Invalid scales are rejected.
- [ ] Scale 1.0 pixel tests cover primitives, components, and full cards.
- [ ] Fractional/integer scale matrix passes without text reflow.
- [ ] GPU state, pipelines, caches, and buffers are reused.
- [ ] Ordinary warm renders create no atlas/page and upload no resident asset.
- [ ] The renderer service has bounded backpressure.
- [ ] Readback padding and channel order are tested.
- [ ] Failure bundles are produced for pixel mismatches.
- [ ] Release workspace tests pass.
- [ ] Representative scale 1.0 and 5.0 outputs are visually inspected.
- [ ] Cold/warm latency and memory are recorded before switching defaults.

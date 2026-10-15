# CR3 camera-preview import

PhotoCraft opens the camera-rendered JPEG stored inside a supported CR3
container. It does not decode CRX sensor data; the result is an 8-bit preview,
not a developed RAW image. The import warning keeps this distinction visible.

The container reader uses bounded ISO BMFF sample tables and PRVW boxes to find
previews, never JPEG-signature searches through compressed sensor data. It also
reads CMT1–4 TIFF blocks, bounded per-frame CTMD metadata and the container XMP
packet. EXIF values and child-IFD pointers are relocated when those independent
blocks are combined. Unknown scalar tags and GPS data are retained, with size,
recursion and cyclic-pointer checks. Damaged metadata produces a warning when
the JPEG can still be opened.

An embedded JPEG ICC profile takes precedence. Without one, the preview is tagged
sRGB or Adobe RGB only when the camera metadata declares it. EXIF's uncalibrated
colour-space value alone is not treated as Adobe RGB. JPEG orientation takes
precedence over container orientation, and the applied orientation is normalized
so saving and reopening does not turn the image a second time.

The reader contains no CRX entropy/wavelet decoder, camera-response calibration
fits or numerical tables obtained from another RAW decoder. It implements
container metadata handling using PhotoCraft's existing TIFF primitives.

Sources:

- ISO BMFF box/sample-table framing and [Laurent Clévy's format description](https://github.com/lclevy/canon_cr3/blob/master/readme.md), used for container layouts.
- [ExifTool's Canon tag documentation](https://exiftool.org/TagNames/Canon.html), used for metadata tag meanings.

Synthetic regression tests cover independent EXIF/GPS/maker blocks, relocation,
unknown tags, endian conversion, cycle/size limits, truncation, colour-profile
precedence, orientation and native-format metadata persistence. No camera
photographs or external reference-decoder binaries are required by these tests.

## Optional HEVC previews

With the existing `heif` feature, a camera-rendered HEVC preview can also open.
Canon's length-prefixed preview cells are placed into a standard HEIF item/grid
container and passed to PhotoCraft's existing decoder. This adapter reads
preview framing and metadata, not CRX entropy or wavelet coefficients.

Eight-bit and ten-bit previews are distinguished from native RAW data. Ten-bit
samples remain in a 16-bit document, rather than being reduced to eight bits.
Supported nclx declarations are sRGB and Rec.2020/ST 2084 PQ; matching ICC tagging
keeps the display transform consistent with the declared pixel encoding.
Unsupported declarations are reported instead of being assumed to be sRGB.

Prefer the larger supported preview, retaining a valid JPEG when optional HEVC
support is absent or the HEVC preview cannot decode. HEVC-only files explain a
missing decoder in builds without `heif`. Crop geometry, working-memory limits,
cell/slice counts and parent-box extents are checked before decoding. The
original preview precision and camera-rendered nature remain explicit in warnings.

Synthetic regression coverage uses the permissively licensed 4:2:2 vector in
`crates/heif/tests/fixtures/`; it requires the dependency correction in #5921.
It verifies cropping, orientation, preserved decoded samples, ten-bit storage,
PQ tagging, metadata persistence, no-HEIF behaviour and JPEG fallback. There is
no camera calibration dataset or full-sensor reference decoder in this path.

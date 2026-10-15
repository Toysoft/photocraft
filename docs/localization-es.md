# Spanish terminology and dynamic UI labels

This note goes with the review and completion of the Spanish catalog
(`crates/ui-egui/src/i18n/es.tsv`). It already covered every key measured by
`cargo xtask i18n-coverage`; the terminology review aligned its wording with the vocabulary a
Spanish-speaking image editor already knows, fixed mistranslations, and made the register
consistent. A later pass added the 1,276 runtime-only labels it still lacked. No proprietary
translation resources, fonts, screenshots or user documents are included: every translation was
written from the meaning of PhotoCraft's own English label.

## Register and style

- **Formal "usted"**, as in Photoshop's Spanish UI: «Haga clic», «Seleccione», «¿Desea guardar…?».
  The catalog mixed «usted» and «tú»; every user-facing instruction now uses «usted».
- **Sentence case** after the first word, except panel and tool names, which keep their capital
  as in Photoshop: «Panel Carácter», «Herramienta Pluma».
- Quotation marks are «angulares»: «{name}».
- Neutral Spanish, matching Adobe's single Spanish catalog (for example «coma flotante»).

## Dynamic UI coverage

After merging the newer upstream catalog rows, the catalog has 4,010 entries. It covers the
complete Italian runtime key set and retains two Spanish profile-prompt labels that other
catalogs also use. The additional labels include filter and adjustment parameters and choices,
Filter Gallery names, Camera Raw, Lighting Effects, History and Layers controls, metadata,
dialogs, status wrappers and System Info labels. Before this pass, those controls fell back to
English because many are assembled dynamically and cannot be found by the static coverage scan.

`spanish_covers_generated_filter_options_and_gallery_names` in `filter_dialog.rs` enumerates the
generated filter and adjustment surface and fails if a future label has no Spanish row. A few
unused legacy labels are retained so the complete catalogs stay aligned.

## Terminology

| English source | Spanish UI | Was |
|---|---|---|
| Artboard | Mesa de trabajo | Lienzo (clashed with *Canvas*) |
| Pattern (fill, overlay, stamp) | Motivo | Patrón |
| Preset | Ajuste preestablecido | Preajuste |
| Keyboard Shortcuts | Métodos abreviados de teclado | Atajos de teclado |
| Grid | Cuadrícula | Rejilla |
| Slice | Sector | Sección |
| Plug-ins | Plugins | Complementos |
| Scratch Disks | Discos de memoria virtual | Discos temporales |
| Puppet Warp | Deformación de posición libre | Deformación de clavos |
| Pin (Puppet Warp) | Chincheta | Pin |
| Dodge / Burn (tools) | Sobreexponer / Subexponer | **swapped** |
| Color Dodge / Color Burn | Sobreexposición / Subexposición de color | **swapped** |
| Linear Dodge (Add) / Linear Burn | Sobreexposición lineal (añadir) / Subexposición lineal | |
| Hard Light / Pin Light / Hard Mix | Luz fuerte / Luz focal / Mezcla definida | Luz dura / Luz puntual / Mezcla dura |
| Pass Through (group) | Pasar a través | Herencia |
| Clone Stamp / Pattern Stamp | Tampón de clonar / Tampón de motivo | Sello… |
| Feather | Calar | Desvanecer (clashed with *Fade*) |
| Fade | Transición | Desvanecer |
| Color Range | Gama de colores | Intervalo de colores |
| Select and Mask | Seleccionar y aplicar máscara | Selección y máscara |
| Selective Color | Corrección selectiva | Color selectivo |
| Shadows/Highlights; Highlights | Sombras/iluminaciones; Iluminaciones | Sombras/luces; Luces |
| Color Lookup | Consulta de colores | Búsqueda de color |
| Content-Aware Fill / Scale / Move | Relleno / Escala según el contenido; Mover con detección de contenido | Relleno de contenido… |
| Adaptive Wide Angle | Gran angular adaptable | Ángulo ancho adaptativo |
| Constrain proportions | Restringir proporciones | **Construir** proporciones |
| Render (filter menu) | Interpretar | Renderizar |
| Smart Sharpen | Enfoque suavizado | Enfoque inteligente |
| Blend If | Fusionar si | Combinar si |
| Knockout (blending options) | Extraer | Cobertura |
| Lights / Darks (Camera Raw) | Claros / Oscuros | Tonos claros / Tonos oscuros |
| Master (Hue/Saturation) | Todos | General |

Menu names that are also actions keep Photoshop's distinction: **Vista** (the *View* menu, not
«Ver»), **Volver** (*Revert*), **Borrar** (*Clear*), **Cortar** (*Trim*, unlike *Crop* «Recortar»),
**Descubrir** (*Reveal*), **Purgar** (*Purge*). Product and technology names (Photomerge, Camera
Raw, droplet, OpenType, sRGB, Wayland) are not translated.

## Out of scope

Engine-generated error details, status text and parameter help that do not pass through the UI
catalog remain English in every language (see
[`localization-parity.md`](localization-parity.md)). User document, layer and preset names and
automation command IDs also remain unchanged. The dynamic completion was contributed by
@gorevyoneticisi with OpenAI Codex under MIT OR Apache-2.0 terms.

## Native review

2026-10-14: a native Spanish speaker (@Daniels-123) audited the catalog, fixed mistranslations and terminology clashes against Photoshop's Spanish UI:
- **Blend modes**: corrected `Screen` from «Pantalla» to «Trama».
- **Edit & Image menus**: corrected `Fade` from «Desvanecer» to «Transición» (and `Fade steps` to «Pasos de transición»), `HDR Toning` from «Tonificación HDR» to «Virado de HDR», `Paste in Place` from «Pegar en contexto» to «Pegar en el mismo lugar», and `Scale` to «Escala».
- **Layers & Panels**: corrected `Essentials` workspace from «Esenciales» to «Aspectos esenciales», `Clone Source` from «Origen de clonado» to «Origen de clonación», `Histories` in Purge from «Historias» to «Historia», `Select and Mask` from «Selección y máscara…» to «Seleccionar y aplicar máscara…», layer navigation from colloquial «capa de arriba/abajo» to «capa siguiente/anterior», and export submenus from «… en archivos…» to «… a archivos…».
- **Gradients & Filters**: corrected gradient stops from «paradas» to «puntos de degradado», and filter names `Shear` («Curvar…»), `Twirl` («Molinete…»), `Pointillize` («Puntillismo…»), `Box Blur` («Desenfoque de rectángulo…»), and `Spin Blur` («Desenfoque de giro…»).
- **Interactive Prompts**: resolved remaining tuteo/imperative inconsistencies into formal "usted", fixed residual English keys («Delete» -> «Supr»), and aligned «cuadrícula» and «motivo».


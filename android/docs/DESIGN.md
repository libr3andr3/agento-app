# agente — sistema de diseño (v2 «warm paper»)

Cada pantalla se lee como UNA app — y NO como Android de fábrica. Construye
estrictamente con estos tokens — un hex en duro, un dp ad-hoc o una
referencia a un estilo `Widget.Material3.*` en un layout es un defecto.

## Tipografía
Plus Jakarta Sans, incluida en `res/font/` (`@font/jakarta`, mapeada por peso
400/500/600/700/800). El theme la aplica globalmente y sobreescribe cada
atributo `textAppearance*` — los layouts usan `?attr/textAppearance…` y
reciben Jakarta gratis. Desde código, usa `R.style.TextAppearance_Agente_*`
(nunca `TextAppearance_Material3_*` — eso resucita Roboto).

Escala: Display/HeadlineMedium 800 (tracking −2%) · Headline/TitleLarge 700 ·
Title 600 · Body 400 · Label 600.

## Color (usa `@color/…`, definidos en res/values/colors.xml)
Los NOMBRES de los tokens cargan significado (Kotlin los referencia); los
valores son el theme.
- `agente_primary` #0B7B5B (esmeralda) — acciones, estados activos
- `agente_primary_container` #DDF3E7 / `agente_on_primary_container` #0A4632
- `agente_secondary` #A6650A (ámbar) + `agente_secondary_container` #FCEED3 — pendientes/avisos
- `agente_error` #BA342A + `agente_error_container` #FBEAE7 — vencido/apagado
- `agente_surface` #F6F5F0 — fondo de pantalla marfil cálido
- `agente_surface_card` #FFFFFF — tarjetas flotantes
- `agente_surface_variant` #ECEAE2 — chips inactivos, burbujas del agente
- `agente_on_surface` #1B1F1C / `agente_on_surface_muted` #6D726C
- `agente_outline` #E4E2D9 — solo trazos de inputs (las tarjetas van sin trazo)
- v2: `agente_hero_grad_start/end` (momentos de marca en degradado),
  `agente_halo` (lavado esmeralda al 10%), `agente_shadow`, `agente_input_bg`,
  `agente_input_stroke` (state list en res/color: outline → esmeralda al foco)

## Forma y espacio (dims en res/values/design.xml)
- espaciado: `space_xs` 4 · `space_s` 8 · `space_m` 16 · `space_l` 24 · `space_xl` 32
- esquinas: tarjetas `corner_card` 24dp · controles `corner_control` 26dp
  (píldora en 52–56dp) · burbujas `corner_bubble` 20dp · sheets `corner_sheet` 28dp
- Tarjetas: el default del theme = `Widget.Agente.Card` (blanca, sin trazo,
  `card_elevation` 2dp de sombra suave). Los banners tintados
  (trial/apagado/gap) van PLANOS: `cardElevation 0dp`, `strokeWidth 0dp`
  explícitos.
- Botones: el MaterialButton por defecto del theme = píldora esmeralda
  (`Widget.Agente.Button`); variantes `.Text`, `.Tonal`, `.Glyph` (iconos de
  composición cuadrados de 48dp). CTAs primarios `cta_height` 56dp.
- Chips: `Widget.Agente.Chip` — píldora sin trazo, colores enlazados desde
  código con los tokens de contenedor.
- Inputs: `Widget.Agente.TextInput` (relleno blanco, radio 18dp, trazo fino →
  esmeralda al foco, sin label flotante) para campos estilo chat; los
  formularios de registro conservan OutlinedBox con labels flotantes y radios
  `corner_control`.

## Momentos de marca en degradado (úsalos con mesura — ESTOS son la marca)
`bg_brand_mark` (squircle de bienvenida) · `bg_hero_halo` (lavado radial
detrás) · `bg_earn_hero` (tarjeta de ingresos del panel) · `bg_done_sheet`
(celebración). Degradado esmeralda a 315°, texto blanco (#B3FFFFFF para
etiquetas secundarias).

## Voz y tono
Primero en español, cálido, corto. Emojis con mesura (uno por superficie, con
propósito). Cada estado vacío enseña, cada estado de error ofrece el
siguiente paso, cada estado de carga es visible (indicador de progreso, CTA
deshabilitado con spinner).

## Reglas duras
- NADA de dependencias gradle nuevas. Disponibles: material 1.12 (M3
  completo), appcompat, recyclerview, constraintlayout si está listado,
  exifinterface.
- themes.xml / colors.xml / design.xml / font/ SON el sistema de diseño:
  cámbialos deliberadamente, en su propio PR, nunca como efecto colateral de
  una pantalla.
- Los strings nuevos van en archivos por área:
  res/values/strings_<area>.xml (español) +
  res/values-en/strings_<area>.xml (inglés).
- Se mantiene una Activity por superficie (sin Compose, sin reescritura a
  nav-graph).
- Accesibilidad: contentDescription en cada control de solo icono; áreas
  táctiles ≥48dp; contraste de texto ≥4.5:1 sobre su superficie.

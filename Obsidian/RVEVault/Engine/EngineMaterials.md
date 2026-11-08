# EngineMaterials

Parent: [[Index]]

## Назначение и границы

engine-scene задаёт форматы .rvemat и .rveshader без зависимости от renderer. engine-render превращает Surface GLSL в Vulkan pipelines во время создания shader resource. engine-runtime и rve-editor связывают эти форматы с GPU-материалами; rve-build проверяет и упаковывает зависимости, но не компилирует GLSL. Контракт сцены и per-FBX-part слотов описан также в [[Engine/EngineScene]], renderer API — в [[Engine/EngineRender]].

## Источники

| Файл | Значимый символ | Роль |
|---|---|---|
| engine/engine-scene/src/material_asset.rs | MaterialAsset, ResolvedMaterialAsset | TOML-формат .rvemat, зависимости, разрешение свойств по shader schema |
| engine/engine-scene/src/shader_asset.rs | ShaderAsset, ShaderSchema, ShaderProperty | Заголовок и GLSL-код .rveshader, проверка схемы и назначение фиксированных слотов |
| engine/engine-render/src/surface_shader.rs | SurfaceShaderSource, compile_surface_shader | Runtime GLSL → SPIR-V через naga без Vulkan device; line mapping ошибок |
| engine/engine-render/shaders/mesh.frag | surface(inout Surface) insertion point | Общий кадр, освещение и шаблон для встроенного Lit и пользовательских surface-функций |
| engine/engine-runtime/src/scene.rs, engine/engine-runtime/src/prefabs.rs | подготовка render assets и GPU upload | Загрузка .rvemat/.rveshader, shader cache и magenta fallback |
| tools/rve-build/src/project.rs | material dependency collection | Проверка материала и упаковка рекурсивных shader/texture dependencies |
| editor/src/materials.rs, editor/src/ui/asset_inspector.rs, editor/src/ui/material_preview.rs | MaterialLibrary и asset UI | Кэш материалов/шейдеров, инспектор и отдельный viewport предпросмотра |

## Формат .rvemat

Файл — строгий TOML-документ с необязательным shader, alpha_mode (opaque, mask, blend), alpha_cutoff (конечный [0, 1], default 0.5), double_sided (default false) и таблицей [properties]. Без shader применяется встроенная Lit schema renderer. Значения свойств — конечное число, массив из 2–4 конечных чисел либо строка с canonical virtual path. Некорректные TOML, диапазоны и virtual paths дают MaterialAssetError с путём и, когда доступно, строкой.

Встроенная Lit schema предоставляет base_color, metallic, roughness, emissive, opacity, normal_scale, occlusion_strength и карты base_color_texture, metallic_roughness_texture, normal_texture, emissive_texture, occlusion_texture. Цветовые данные используют sRGB, карты параметров — linear. Для выбранной schema отсутствующее или несовместимое значение получает объявленный default и предупреждение; ключи, которых нет в schema, сохраняются при редактировании, но не влияют на результат и тоже предупреждаются.

При назначении слота Lit .rvemat задаёт материал вместо FBX material этой части. Затем применяется MeshRenderer.texture как замена base-color, затем заданные поля legacy MeshRenderer.material. Если слот пуст, сохраняется FBX material и тот же texture/override порядок. Пользовательский shader применяет только объявленные properties; legacy MeshRenderer.texture и MeshRenderer.material к нему не применяются.

MaterialAsset::dependencies() возвращает уникальные пути shader и texture properties. В runtime каждый dependency читается через project ResourceHandle; в сцене материал назначается через MeshRenderer.materials. Ошибка чтения или разбора .rvemat, отсутствующий dependency, ошибка GLSL compile или GPU resource creation журналируется, а часть рисуется общим magenta Lit error material без panic. Редактор сохраняет последнюю успешную GPU-версию при неудачном hot reload.

## Формат .rveshader

Файл состоит из TOML header и GLSL body, разделённых строкой ---. Header содержит [shader] с lighting = "lit" (default) или "unlit" и упорядоченные [[properties]]:

- name — уникальный GLSL identifier; label необязателен и по умолчанию получается из name;
- type — float, vec2, vec3, vec4, color или texture; default, min и max задают default и диапазон чисел;
- texture property может задавать color_space (srgb/linear), fallback (white/black/normal) и путь default texture.

Поддерживаются до 16 числовых и 8 texture properties. Их независимые слоты назначаются в порядке объявления. Имена проверяются на дубли, зарезервированные идентификаторы и GLSL validity; texture-specific поля запрещены у числовых свойств.

GLSL body определяет void surface(inout Surface s). Surface несёт albedo, emissive, metallic, roughness, opacity, normal, occlusion, uv и position; renderer предоставляет объявленные числовые имена как макросы и функцию sample_<name>(uv) для texture properties. Naga собирает фрагмент в порядке helpers → defines → surface body → #undef → main, чтобы макросы не затрагивали встроенный main. Встроенная Lit реализация после выделения этой точки сохраняет прежнюю формулу и результат.

## Renderer и время жизни

Renderer::create_surface_shader(&SurfaceShaderSource) компилирует SPIR-V для alpha Opaque, Mask и Blend, после чего создаёт четыре graphics pipelines: opaque, opaque-after-depth-prepass, mask и blend. Частичный сбой уничтожает уже созданные shader/pipeline объекты. Выбор pipeline учитывает ShaderId, alpha mode и проход; wireframe и overdraw debug draws используют встроенные pipelines.

Material.shader: Option<ShaderId>, user_values: [[f32; 4]; 16] и user_textures: [Option<TextureId>; 8] передают выбранный Surface Shader и его фиксированные данные. create_material проверяет shader id, layout slots, texture ids и color spaces; нарушение shader-контракта возвращает RenderError::InvalidShader, а material/texture ошибки — InvalidMaterial. destroy_surface_shader возвращает ошибку, пока хоть один живой material ссылается на этот shader. Сначала уничтожаются такие materials, затем shader.

Пользовательский shader проходит через тот же Lit lighting path при lighting = "lit" либо не добавляет освещение при unlit. Ошибка shader/material не уничтожает прежнюю валидную GPU-версию при editor reload; для нового слота runtime и editor показывают magenta Lit error material и пишут ошибку в log/Console. Этот fallback принадлежит потребителям renderer, не renderer API.

## Сборка и ограничения

Встроенные GLSL shaders компилируются engine-render/build.rs во время сборки. Пользовательские .rveshader компилирует naga GLSL frontend во время создания surface shader; Vulkan SDK и glslc не нужны. rve-build проверяет структуру .rveshader и зависимости материалов, но намеренно не зависит от engine-render и не компилирует пользовательский GLSL. Поэтому синтаксическая ошибка в body обнаруживается редактором/runtime при загрузке, а builder может упаковать её; потребитель заменит material на error material и запишет диагностику.

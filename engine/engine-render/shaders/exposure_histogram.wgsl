// Гистограмма log2-яркости HDR-источника: 256 бинов в диапазоне [MIN_LOG2, MAX_LOG2].
// Бин 0 содержит пиксели с нулевой яркостью; локальная гистограмма группы сливается в общую.

const MIN_LOG2: f32 = -10.0;
const MAX_LOG2: f32 = 4.0;

struct Exposure {
    histogram: array<atomic<u32>, 256>,
    adapted_ev: f32,
    pad0: f32,
    pad1: f32,
    pad2: f32,
}

@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var<storage, read_write> exposure: Exposure;

var<workgroup> local_bins: array<atomic<u32>, 256>;

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) id: vec3<u32>, @builtin(local_invocation_index) index: u32) {
    atomicStore(&local_bins[index], 0u);
    workgroupBarrier();
    let size = textureDimensions(source);
    if (id.x < size.x && id.y < size.y) {
        let color = textureLoad(source, vec2<i32>(id.xy), 0).rgb;
        let luminance = dot(color, vec3<f32>(0.2126, 0.7152, 0.0722));
        var bin = 0u;
        if (luminance > 0.0) {
            let log_luminance = clamp(log2(luminance), MIN_LOG2, MAX_LOG2);
            bin = u32(clamp((log_luminance - MIN_LOG2) / (MAX_LOG2 - MIN_LOG2) * 256.0, 0.0, 255.0));
        }
        atomicAdd(&local_bins[bin], 1u);
    }
    workgroupBarrier();
    atomicAdd(&exposure.histogram[index], atomicLoad(&local_bins[index]));
}

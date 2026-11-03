#version 450

layout(location = 0) in vec3 in_normal;
layout(location = 1) in vec2 in_uv;
layout(location = 2) in vec3 in_color;
layout(location = 3) in vec4 in_shadow_clip[4];
layout(location = 7) in vec3 in_world_position;

layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform FrameUniforms {
    mat4 view_projection;
    mat4 inverse_view_projection;
    mat4 shadow_view_projections[4];
    vec4 light_direction;
    vec4 light_color;
    vec4 ambient_color;
    vec4 shadow_params;
    vec4 camera_position;
    mat4 environment_rotation;
    vec4 environment_params;
    vec4 local_light_params;
} frame;

layout(set = 0, binding = 1) uniform texture2DArray shadow_map;
layout(set = 0, binding = 2) uniform samplerShadow shadow_sampler;

struct LocalLightData {
    vec4 position_range;
    vec4 direction_type;
    vec4 color_intensity;
    vec4 cone_params;
};

layout(set = 0, binding = 3, std430) readonly buffer LocalLightBuffer {
    LocalLightData lights[];
} local_light_buffer;

layout(set = 1, binding = 0) uniform MaterialUniforms {
    vec4 base_color;
    vec4 parameters;
    vec4 emissive;
    vec4 alpha_parameters;
} material;
layout(set = 1, binding = 1) uniform texture2D base_color_texture;
layout(set = 1, binding = 2) uniform texture2D metallic_roughness_texture;
layout(set = 1, binding = 3) uniform texture2D normal_texture;
layout(set = 1, binding = 4) uniform texture2D emissive_texture;
layout(set = 1, binding = 5) uniform texture2D occlusion_texture;
layout(set = 1, binding = 6) uniform sampler material_sampler;

layout(set = 2, binding = 0) uniform textureCube environment_map;
layout(set = 2, binding = 1) uniform sampler environment_sampler;
layout(set = 2, binding = 2) uniform textureCube irradiance_map;
layout(set = 2, binding = 3) uniform textureCube prefiltered_map;
layout(set = 2, binding = 4) uniform texture2D brdf_lut;
layout(set = 2, binding = 5) uniform sampler brdf_sampler;

const float PI = 3.14159265358979323846;

vec3 safe_normalize(vec3 value, vec3 fallback) {
    float length_squared = dot(value, value);
    if (isnan(length_squared) || isinf(length_squared) || length_squared <= 1e-20) {
        return fallback;
    }
    return value * inversesqrt(length_squared);
}

float distribution_ggx(float normal_dot_half_input, float alpha) {
    float normal_dot_half = clamp(normal_dot_half_input, 0.0, 1.0);
    float alpha_squared = alpha * alpha;
    float denominator = (1.0 - normal_dot_half * normal_dot_half)
        + alpha_squared * normal_dot_half * normal_dot_half;
    denominator = max(denominator, alpha_squared);
    return alpha_squared / (PI * denominator * denominator);
}

float visibility_smith_ggx_correlated(float normal_dot_light, float normal_dot_view, float alpha) {
    float alpha_squared = alpha * alpha;
    float lambda_view = normal_dot_light * sqrt(max(
        normal_dot_view * normal_dot_view * (1.0 - alpha_squared) + alpha_squared,
        0.0
    ));
    float lambda_light = normal_dot_view * sqrt(max(
        normal_dot_light * normal_dot_light * (1.0 - alpha_squared) + alpha_squared,
        0.0
    ));
    return 0.5 / max(lambda_view + lambda_light, 1e-6);
}

vec3 fresnel_schlick(float view_dot_half, vec3 f0) {
    float factor = pow(1.0 - clamp(view_dot_half, 0.0, 1.0), 5.0);
    return f0 + (1.0 - f0) * factor;
}

vec3 direct_brdf(
    vec3 light,
    vec3 normal,
    vec3 view,
    vec3 f0,
    vec3 base_color,
    float metallic,
    float alpha,
    float normal_dot_view
) {
    vec3 half_vector = safe_normalize(view + light, normal);
    float normal_dot_light = max(dot(normal, light), 0.0);
    float normal_dot_half = max(dot(normal, half_vector), 0.0);
    vec3 fresnel = fresnel_schlick(max(dot(half_vector, view), 0.0), f0);
    float distribution = distribution_ggx(normal_dot_half, alpha);
    float visibility = visibility_smith_ggx_correlated(
        normal_dot_light,
        normal_dot_view,
        alpha
    );
    vec3 diffuse_color = (1.0 - metallic) * base_color;
    vec3 diffuse = (1.0 - fresnel) * diffuse_color / PI;
    vec3 specular = distribution * visibility * fresnel;
    return (diffuse + specular) * normal_dot_light * PI;
}

float range_attenuation(float distance_squared, float range) {
    float normalized_distance = sqrt(distance_squared) / range;
    float window = clamp(1.0 - pow(normalized_distance, 4.0), 0.0, 1.0);
    return window * window / max(distance_squared, 1e-4);
}

float spot_attenuation(
    vec3 light_ray,
    vec3 direction,
    float cos_inner_angle,
    float cos_outer_angle
) {
    float ray_cosine = dot(light_ray, direction);
    if (cos_inner_angle <= cos_outer_angle + 1e-6) {
        return ray_cosine >= cos_outer_angle ? 1.0 : 0.0;
    }
    return smoothstep(cos_outer_angle, cos_inner_angle, ray_cosine);
}

void main() {
    vec4 base_sample = texture(sampler2D(base_color_texture, material_sampler), in_uv);
    float surface_alpha = base_sample.a * material.alpha_parameters.x;
#ifdef ALPHA_MASK
    if (surface_alpha < material.alpha_parameters.y) {
        discard;
    }
    float output_alpha = 1.0;
#elif defined(ALPHA_BLEND)
    float output_alpha = surface_alpha;
#else
    float output_alpha = 1.0;
#endif
    vec4 metallic_roughness_sample = texture(
        sampler2D(metallic_roughness_texture, material_sampler), in_uv
    );
    vec3 normal_sample = texture(sampler2D(normal_texture, material_sampler), in_uv).rgb;
    vec3 emissive_sample = texture(sampler2D(emissive_texture, material_sampler), in_uv).rgb;
    float occlusion = texture(sampler2D(occlusion_texture, material_sampler), in_uv).r;

    vec3 dp1 = dFdx(in_world_position);
    vec3 dp2 = dFdy(in_world_position);
    vec2 duv1 = dFdx(in_uv);
    vec2 duv2 = dFdy(in_uv);

    vec3 geometric_normal = safe_normalize(in_normal, vec3(0.0, 0.0, 1.0));
    vec3 tangent = vec3(0.0);
    vec3 bitangent = vec3(0.0);
    float determinant = duv1.x * duv2.y - duv1.y * duv2.x;
    bool valid_basis = !isnan(determinant) && !isinf(determinant) && abs(determinant) > 1e-8;
    if (valid_basis && !any(isnan(dp1)) && !any(isinf(dp1))
        && !any(isnan(dp2)) && !any(isinf(dp2))) {
        vec3 raw_tangent = (dp1 * duv2.y - dp2 * duv1.y) / determinant;
        vec3 raw_bitangent = -(dp2 * duv1.x - dp1 * duv2.x) / determinant;
        raw_tangent -= geometric_normal * dot(raw_tangent, geometric_normal);
        raw_bitangent -= geometric_normal * dot(raw_bitangent, geometric_normal);
        float tangent_length = dot(raw_tangent, raw_tangent);
        float bitangent_length = dot(raw_bitangent, raw_bitangent);
        valid_basis = !any(isnan(raw_tangent)) && !any(isinf(raw_tangent))
            && !any(isnan(raw_bitangent)) && !any(isinf(raw_bitangent))
            && !isnan(tangent_length) && !isinf(tangent_length)
            && !isnan(bitangent_length) && !isinf(bitangent_length)
            && tangent_length > 1e-20 && bitangent_length > 1e-20;
        if (valid_basis) {
            tangent = safe_normalize(raw_tangent, geometric_normal);
            bitangent = safe_normalize(raw_bitangent, geometric_normal);
        }
    }

    vec3 tangent_normal = normal_sample * 2.0 - 1.0;
    tangent_normal.xy *= material.parameters.z;
    tangent_normal = safe_normalize(tangent_normal, vec3(0.0, 0.0, 1.0));
    vec3 normal = geometric_normal;
    if (material.parameters.w > 0.5 && valid_basis) {
        normal = safe_normalize(
            tangent * tangent_normal.x + bitangent * tangent_normal.y
                + geometric_normal * tangent_normal.z,
            geometric_normal
        );
    }

    vec3 base_color = base_sample.rgb * material.base_color.rgb * in_color;
    float metallic = clamp(material.parameters.x * metallic_roughness_sample.b, 0.0, 1.0);
    float roughness = clamp(material.parameters.y * metallic_roughness_sample.g, 0.0, 1.0);
    float effective_roughness = max(roughness, 0.045);
    float alpha = effective_roughness * effective_roughness;
    vec3 f0 = mix(vec3(0.04), base_color, metallic);

    vec3 view = safe_normalize(frame.camera_position.xyz - in_world_position, geometric_normal);
    float normal_dot_view = max(dot(normal, view), 1e-4);
    vec3 diffuse_color = (1.0 - metallic) * base_color;
    vec3 directional_light = safe_normalize(-frame.light_direction.xyz, geometric_normal);
    float geometric_light_dot = dot(geometric_normal, directional_light);
    float shadow = 1.0;
    if (frame.shadow_params.x > 0.5) {
        for (int cascade = 0; cascade < int(frame.shadow_params.z); ++cascade) {
            vec4 light_clip = in_shadow_clip[cascade];
            if (light_clip.w == 0.0) {
                continue;
            }
            vec3 light_ndc = light_clip.xyz / light_clip.w;
            vec2 shadow_uv = vec2(0.5 + 0.5 * light_ndc.x, 0.5 - 0.5 * light_ndc.y);
            if (any(lessThan(shadow_uv, vec2(0.0)))
                || any(greaterThan(shadow_uv, vec2(1.0)))
                || light_ndc.z < 0.0 || light_ndc.z > 1.0) {
                continue;
            }
            float bias = max(0.005 * (1.0 - max(geometric_light_dot, 0.0)), 0.0005);
            float cascade_shadow = 0.0;
            for (int y = -1; y <= 1; ++y) {
                for (int x = -1; x <= 1; ++x) {
                    vec2 offset = vec2(float(x), float(y)) * frame.shadow_params.y;
                    cascade_shadow += texture(
                        sampler2DArrayShadow(shadow_map, shadow_sampler),
                        vec4(shadow_uv + offset, float(cascade), light_ndc.z - bias)
                    );
                }
            }
            shadow = cascade_shadow / 9.0;
            break;
        }
    }

    vec3 direct = direct_brdf(
        directional_light,
        normal,
        view,
        f0,
        base_color,
        metallic,
        alpha,
        normal_dot_view
    ) * frame.light_color.rgb * shadow;
    int local_light_count = clamp(
        int(frame.local_light_params.x + 0.5),
        0,
        MAX_FORWARD_LIGHTS
    );
    for (int light_index = 0; light_index < local_light_count; ++light_index) {
        LocalLightData local_light = local_light_buffer.lights[light_index];
        vec3 light_delta = local_light.position_range.xyz - in_world_position;
        float distance_squared = dot(light_delta, light_delta);
        vec3 local_light_direction = safe_normalize(light_delta, geometric_normal);
        float attenuation = range_attenuation(
            distance_squared,
            local_light.position_range.w
        );
        if (local_light.direction_type.w > 0.5) {
            attenuation *= spot_attenuation(
                -local_light_direction,
                local_light.direction_type.xyz,
                local_light.cone_params.x,
                local_light.cone_params.y
            );
        }
        direct += direct_brdf(
            local_light_direction,
            normal,
            view,
            f0,
            base_color,
            metallic,
            alpha,
            normal_dot_view
        ) * local_light.color_intensity.rgb * attenuation;
    }
    vec3 reflected = reflect(-view, normal);
    mat3 world_to_environment = transpose(mat3(frame.environment_rotation));
    vec3 environment_normal = world_to_environment * normal;
    vec3 environment_reflection = world_to_environment * reflected;
    vec3 irradiance = textureLod(
        samplerCube(irradiance_map, environment_sampler), environment_normal, 0.0
    ).rgb;
    float prefilter_lod = effective_roughness * 5.0;
    vec3 prefiltered = textureLod(
        samplerCube(prefiltered_map, environment_sampler), environment_reflection, prefilter_lod
    ).rgb;
    vec2 brdf = texture(
        sampler2D(brdf_lut, brdf_sampler), vec2(normal_dot_view, effective_roughness)
    ).rg;
    vec3 indirect_diffuse = irradiance * diffuse_color / PI;
    vec3 indirect_specular = prefiltered * (f0 * brdf.x + brdf.y);
    float environment_weight = frame.environment_params.z;
    vec3 ambient = mix(
        frame.ambient_color.rgb * (indirect_diffuse + indirect_specular),
        frame.environment_params.x * (indirect_diffuse + indirect_specular),
        environment_weight
    ) * mix(1.0, occlusion, material.alpha_parameters.w);
    vec3 emissive = material.emissive.rgb * emissive_sample;
    out_color = vec4(ambient + direct + emissive, output_alpha);
}

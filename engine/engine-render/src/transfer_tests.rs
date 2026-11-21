//! Выбор transfer-семейства и планирование барьеров передачи владения без Vulkan-устройства.

use super::{
    OwnershipTransfer, TextureRole, UploadQueues, color_mip_subresource_range,
    device_queue_families, image_upload_barriers, mesh_upload_barriers,
    select_transfer_queue_family, split_buffer_barrier, split_image_barrier,
    texture_mip_destination_shader_barrier, texture_upload_barrier,
};
use ash::vk;
use ash::vk::Handle as _;

fn family(flags: vk::QueueFlags, queue_count: u32) -> vk::QueueFamilyProperties {
    vk::QueueFamilyProperties {
        queue_flags: flags,
        queue_count,
        ..Default::default()
    }
}

const UNIVERSAL: vk::QueueFlags = vk::QueueFlags::from_raw(
    vk::QueueFlags::GRAPHICS.as_raw()
        | vk::QueueFlags::COMPUTE.as_raw()
        | vk::QueueFlags::TRANSFER.as_raw(),
);
const ASYNC_COMPUTE: vk::QueueFlags =
    vk::QueueFlags::from_raw(vk::QueueFlags::COMPUTE.as_raw() | vk::QueueFlags::TRANSFER.as_raw());

const OWNERSHIP: OwnershipTransfer = OwnershipTransfer {
    src_family: 2,
    dst_family: 0,
};

#[test]
fn single_universal_family_keeps_uploads_on_graphics() {
    // MoltenVK и многие встроенные GPU дают одно семейство на всё.
    assert_eq!(
        select_transfer_queue_family(&[family(UNIVERSAL, 1)], 0),
        None
    );
}

#[test]
fn dedicated_transfer_family_is_preferred_over_async_compute() {
    let families = [
        family(UNIVERSAL, 16),
        family(ASYNC_COMPUTE, 8),
        family(vk::QueueFlags::TRANSFER, 2),
    ];
    assert_eq!(select_transfer_queue_family(&families, 0), Some(2));
}

#[test]
fn async_compute_family_with_transfer_is_used_without_dedicated_one() {
    let families = [family(UNIVERSAL, 1), family(ASYNC_COMPUTE, 2)];
    assert_eq!(select_transfer_queue_family(&families, 0), Some(1));
}

#[test]
fn lowest_index_wins_between_equal_transfer_families() {
    let families = [
        family(UNIVERSAL, 1),
        family(vk::QueueFlags::TRANSFER, 1),
        family(vk::QueueFlags::TRANSFER, 1),
    ];
    assert_eq!(select_transfer_queue_family(&families, 0), Some(1));
}

#[test]
fn unsuitable_families_are_rejected() {
    let families = [
        family(UNIVERSAL, 1),
        // Второе graphics-семейство не считается отдельной очередью копирования.
        family(UNIVERSAL, 1),
        // TRANSFER у compute-семейства неявный: без бита в свойствах семейство не берём.
        family(vk::QueueFlags::COMPUTE, 1),
        // Семейство без очередей.
        family(vk::QueueFlags::TRANSFER, 0),
        family(vk::QueueFlags::SPARSE_BINDING, 1),
    ];
    assert_eq!(select_transfer_queue_family(&families, 0), None);
    assert_eq!(select_transfer_queue_family(&[], 0), None);
}

#[test]
fn graphics_family_index_is_never_returned() {
    let families = [family(vk::QueueFlags::TRANSFER, 1)];
    assert_eq!(select_transfer_queue_family(&families, 0), None);
}

#[test]
fn device_creates_one_queue_per_distinct_family() {
    assert_eq!(device_queue_families(0, None).collect::<Vec<_>>(), [0]);
    assert_eq!(
        device_queue_families(0, Some(2)).collect::<Vec<_>>(),
        [0, 2]
    );
    assert_eq!(device_queue_families(1, Some(1)).collect::<Vec<_>>(), [1]);
}

#[test]
fn ownership_transfer_exists_only_for_a_separate_family() {
    let graphics_only = UploadQueues {
        graphics_family: 0,
        transfer_family: None,
    };
    assert_eq!(graphics_only.ownership(), None);
    let same = UploadQueues {
        graphics_family: 1,
        transfer_family: Some(1),
    };
    assert_eq!(same.ownership(), None);
    let separate = UploadQueues {
        graphics_family: 0,
        transfer_family: Some(2),
    };
    assert_eq!(separate.ownership(), Some(OWNERSHIP));
}

fn assert_matching_image_pair(
    release: &vk::ImageMemoryBarrier2<'_>,
    acquire: &vk::ImageMemoryBarrier2<'_>,
) {
    // Спецификация требует одинаковые семейства, layout-ы и диапазон у release и acquire.
    assert_eq!(release.src_queue_family_index, OWNERSHIP.src_family);
    assert_eq!(release.dst_queue_family_index, OWNERSHIP.dst_family);
    assert_eq!(acquire.src_queue_family_index, OWNERSHIP.src_family);
    assert_eq!(acquire.dst_queue_family_index, OWNERSHIP.dst_family);
    assert_eq!(release.old_layout, acquire.old_layout);
    assert_eq!(release.new_layout, acquire.new_layout);
    assert_eq!(release.image, acquire.image);
    let (r, a) = (release.subresource_range, acquire.subresource_range);
    assert_eq!(
        (
            r.aspect_mask,
            r.base_mip_level,
            r.level_count,
            r.base_array_layer,
            r.layer_count
        ),
        (
            a.aspect_mask,
            a.base_mip_level,
            a.level_count,
            a.base_array_layer,
            a.layer_count
        )
    );
    assert_eq!(release.dst_stage_mask, vk::PipelineStageFlags2::NONE);
    assert_eq!(release.dst_access_mask, vk::AccessFlags2::NONE);
    assert_eq!(acquire.src_stage_mask, vk::PipelineStageFlags2::NONE);
    assert_eq!(acquire.src_access_mask, vk::AccessFlags2::NONE);
}

#[test]
fn image_barrier_without_ownership_is_returned_unchanged() {
    let barrier = texture_mip_destination_shader_barrier(
        vk::Image::null(),
        color_mip_subresource_range(0, 3),
    );
    let (release, acquire) = split_image_barrier(barrier, None);
    assert!(release.is_none());
    assert_eq!(acquire.src_stage_mask, barrier.src_stage_mask);
    assert_eq!(acquire.dst_stage_mask, barrier.dst_stage_mask);
    assert_eq!(acquire.src_queue_family_index, vk::QUEUE_FAMILY_IGNORED);
    assert_eq!(acquire.dst_queue_family_index, vk::QUEUE_FAMILY_IGNORED);
}

#[test]
fn image_barrier_splits_into_release_and_acquire() {
    let barrier = texture_mip_destination_shader_barrier(
        vk::Image::null(),
        color_mip_subresource_range(0, 3),
    );
    let (release, acquire) = split_image_barrier(barrier, Some(OWNERSHIP));
    let release = release.expect("release для отдельного семейства");
    assert_matching_image_pair(&release, &acquire);
    assert_eq!(release.src_stage_mask, barrier.src_stage_mask);
    assert_eq!(release.src_access_mask, vk::AccessFlags2::TRANSFER_WRITE);
    assert_eq!(acquire.dst_stage_mask, barrier.dst_stage_mask);
    assert_eq!(
        acquire.dst_access_mask,
        vk::AccessFlags2::SHADER_SAMPLED_READ
    );
}

#[test]
fn buffer_barrier_splits_into_matching_release_and_acquire() {
    let buffer = vk::Buffer::from_raw(7);
    let [vertex, _] = mesh_upload_barriers(buffer, 96, vk::Buffer::from_raw(8), 12, false);
    let (none, same) = split_buffer_barrier(vertex, None);
    assert!(none.is_none());
    assert_eq!(same.src_queue_family_index, vk::QUEUE_FAMILY_IGNORED);

    let (release, acquire) = split_buffer_barrier(vertex, Some(OWNERSHIP));
    let release = release.expect("release для отдельного семейства");
    for barrier in [&release, &acquire] {
        assert_eq!(barrier.src_queue_family_index, OWNERSHIP.src_family);
        assert_eq!(barrier.dst_queue_family_index, OWNERSHIP.dst_family);
        assert_eq!(barrier.buffer, buffer);
        assert_eq!(barrier.offset, 0);
        assert_eq!(barrier.size, 96);
    }
    assert_eq!(release.src_stage_mask, vk::PipelineStageFlags2::COPY);
    assert_eq!(release.dst_stage_mask, vk::PipelineStageFlags2::NONE);
    assert_eq!(acquire.src_stage_mask, vk::PipelineStageFlags2::NONE);
    assert_eq!(
        acquire.dst_stage_mask,
        vk::PipelineStageFlags2::VERTEX_ATTRIBUTE_INPUT
    );
}

#[test]
fn mesh_barriers_cover_vertex_index_and_skinning_reads() {
    let vertex = vk::Buffer::from_raw(1);
    let index = vk::Buffer::from_raw(2);
    let [static_vertex, static_index] = mesh_upload_barriers(vertex, 48, index, 24, false);
    assert_eq!(
        static_vertex.dst_stage_mask,
        vk::PipelineStageFlags2::VERTEX_ATTRIBUTE_INPUT
    );
    assert_eq!(static_vertex.size, 48);
    assert_eq!(static_index.buffer, index);
    assert_eq!(static_index.size, 24);
    assert_eq!(
        static_index.dst_stage_mask,
        vk::PipelineStageFlags2::INDEX_INPUT
    );
    assert_eq!(static_index.dst_access_mask, vk::AccessFlags2::INDEX_READ);

    let [skinned_vertex, _] = mesh_upload_barriers(vertex, 48, index, 24, true);
    assert!(
        skinned_vertex
            .dst_stage_mask
            .contains(vk::PipelineStageFlags2::COMPUTE_SHADER)
    );
    assert!(
        skinned_vertex.dst_access_mask.contains(
            vk::AccessFlags2::SHADER_STORAGE_READ | vk::AccessFlags2::VERTEX_ATTRIBUTE_READ
        )
    );
}

#[test]
fn color_upload_without_transfer_queue_keeps_current_barriers() {
    let image = vk::Image::from_raw(3);
    let plan = image_upload_barriers(image, 5, TextureRole::Color, None);
    let current = texture_upload_barrier(image, 5);
    assert_eq!(plan.before_copy.dst_stage_mask, current.dst_stage_mask);
    assert_eq!(plan.before_copy.subresource_range.level_count, 5);
    assert!(plan.release.is_none());
    // Без передачи владения blit-ы идут сразу за копией в той же command buffer.
    assert!(plan.acquire.is_none());
}

#[test]
fn color_upload_hands_whole_chain_to_graphics_for_blits() {
    let image = vk::Image::from_raw(3);
    for mip_levels in [1, 5] {
        let plan = image_upload_barriers(image, mip_levels, TextureRole::Color, Some(OWNERSHIP));
        let release = plan.release.expect("release Color");
        let acquire = plan.acquire.expect("acquire Color");
        assert_matching_image_pair(&release, &acquire);
        assert_eq!(release.old_layout, vk::ImageLayout::TRANSFER_DST_OPTIMAL);
        assert_eq!(release.new_layout, vk::ImageLayout::TRANSFER_DST_OPTIMAL);
        assert_eq!(release.subresource_range.level_count, mip_levels);
        // Blit-ы mip-ов остаются на graphics queue.
        assert!(
            acquire
                .dst_stage_mask
                .contains(vk::PipelineStageFlags2::BLIT)
        );
    }
}

#[test]
fn normal_upload_publishes_chain_to_shaders_with_and_without_transfer_queue() {
    let image = vk::Image::from_raw(4);
    let local = image_upload_barriers(image, 4, TextureRole::Normal, None);
    assert!(local.release.is_none());
    let acquire = local.acquire.expect("барьер к шейдерам");
    assert_eq!(
        acquire.new_layout,
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
    );
    assert_eq!(acquire.subresource_range.level_count, 4);

    let split = image_upload_barriers(image, 4, TextureRole::Normal, Some(OWNERSHIP));
    let release = split.release.expect("release Normal");
    let acquire = split.acquire.expect("acquire Normal");
    assert_matching_image_pair(&release, &acquire);
    assert_eq!(release.old_layout, vk::ImageLayout::TRANSFER_DST_OPTIMAL);
    assert_eq!(
        release.new_layout,
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
    );
    assert_eq!(
        acquire.dst_access_mask,
        vk::AccessFlags2::SHADER_SAMPLED_READ
    );
}

#[test]
fn transfer_queue_barriers_use_only_copy_stage() {
    // Очередь без GRAPHICS не поддерживает BLIT и шейдерные стадии.
    let allowed = vk::PipelineStageFlags2::COPY;
    for role in [TextureRole::Color, TextureRole::Normal] {
        let plan = image_upload_barriers(vk::Image::from_raw(5), 3, role, Some(OWNERSHIP));
        let release = plan.release.expect("release");
        for barrier in [plan.before_copy, release] {
            assert!(allowed.contains(barrier.src_stage_mask), "{role:?}");
            assert!(allowed.contains(barrier.dst_stage_mask), "{role:?}");
        }
        assert_eq!(plan.before_copy.old_layout, vk::ImageLayout::UNDEFINED);
        // Первое использование: содержимое не нужно, поэтому захвата владения нет.
        assert_eq!(
            plan.before_copy.src_queue_family_index,
            vk::QUEUE_FAMILY_IGNORED
        );
    }
}

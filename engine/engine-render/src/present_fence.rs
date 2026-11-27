//! Bounded pool and CPU-side state for Vulkan presentation fences.

use ash::vk;

pub(crate) const PRESENT_FENCE_CAPACITY: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FenceState {
    Available,
    Reserved,
    Pending(u64),
    Uncertain(u64),
}

struct FenceSlot<F> {
    fence: F,
    state: FenceState,
}

pub(crate) struct PresentFences<F> {
    slots: Vec<FenceSlot<F>>,
    reserved: Option<usize>,
}

impl<F: Copy> PresentFences<F> {
    pub(crate) fn new() -> Self {
        Self {
            slots: Vec::new(),
            reserved: None,
        }
    }

    pub(crate) fn add(&mut self, fence: F) -> bool {
        if self.slots.len() == PRESENT_FENCE_CAPACITY {
            return false;
        }
        self.slots.push(FenceSlot {
            fence,
            state: FenceState::Available,
        });
        true
    }

    pub(crate) fn reserve(&mut self) -> Option<F> {
        if self.reserved.is_some() {
            return None;
        }
        let index = self
            .slots
            .iter()
            .position(|slot| slot.state == FenceState::Available)?;
        self.slots[index].state = FenceState::Reserved;
        self.reserved = Some(index);
        Some(self.slots[index].fence)
    }

    pub(crate) fn finish(
        &mut self,
        disposition: PresentResultDisposition,
        generation: u64,
    ) -> bool {
        let Some(index) = self.reserved.take() else {
            return false;
        };
        self.slots[index].state = match disposition {
            PresentResultDisposition::Enqueued => FenceState::Pending(generation),
            PresentResultDisposition::NotEnqueued => FenceState::Available,
            PresentResultDisposition::Uncertain => FenceState::Uncertain(generation),
        };
        true
    }

    /// Signaled fences become available; callers reset them before the next presentation.
    pub(crate) fn poll<E>(
        &mut self,
        mut is_signaled: impl FnMut(F) -> Result<bool, E>,
    ) -> Result<Vec<(F, u64)>, E> {
        let mut completed = Vec::new();
        for (index, slot) in self.slots.iter().enumerate() {
            let generation = match slot.state {
                FenceState::Pending(generation) | FenceState::Uncertain(generation) => generation,
                FenceState::Available | FenceState::Reserved => continue,
            };
            if is_signaled(slot.fence)? {
                completed.push((index, slot.fence, generation));
            }
        }
        for (index, _, _) in &completed {
            self.slots[*index].state = FenceState::Available;
        }
        Ok(completed
            .into_iter()
            .map(|(_, fence, generation)| (fence, generation))
            .collect())
    }

    pub(crate) fn outstanding(&self, generation: u64) -> usize {
        self.slots
            .iter()
            .filter(|slot| {
                matches!(
                    slot.state,
                    FenceState::Pending(found) | FenceState::Uncertain(found)
                        if found == generation
                )
            })
            .count()
    }

    pub(crate) fn generation_fences(&self, generation: u64) -> Vec<F> {
        self.slots
            .iter()
            .filter_map(|slot| {
                matches!(
                    slot.state,
                    FenceState::Pending(found) | FenceState::Uncertain(found)
                        if found == generation
                )
                .then_some(slot.fence)
            })
            .collect()
    }

    pub(crate) fn oldest_pending_generation(&self) -> Option<u64> {
        self.slots
            .iter()
            .filter_map(|slot| match slot.state {
                FenceState::Pending(generation) | FenceState::Uncertain(generation) => {
                    Some(generation)
                }
                FenceState::Available | FenceState::Reserved => None,
            })
            .min()
    }

    /// Records completion after a successful `vkWaitForFences` for this generation.
    pub(crate) fn complete_generation(&mut self, generation: u64) -> usize {
        let mut completed = 0;
        for slot in &mut self.slots {
            if matches!(
                slot.state,
                FenceState::Pending(found) | FenceState::Uncertain(found)
                    if found == generation
            ) {
                slot.state = FenceState::Available;
                completed += 1;
            }
        }
        completed
    }

    pub(crate) fn waiting_fences(&self) -> Vec<F> {
        self.slots
            .iter()
            .filter_map(|slot| {
                matches!(
                    slot.state,
                    FenceState::Pending(_) | FenceState::Uncertain(_)
                )
                .then_some(slot.fence)
            })
            .collect()
    }

    pub(crate) fn all_fences(&self) -> impl Iterator<Item = F> + '_ {
        self.slots.iter().map(|slot| slot.fence)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PresentResultDisposition {
    Enqueued,
    NotEnqueued,
    Uncertain,
}

pub(crate) fn classify_present_result(
    result: Result<bool, vk::Result>,
) -> PresentResultDisposition {
    match result {
        Ok(_) => PresentResultDisposition::Enqueued,
        Err(
            vk::Result::SUBOPTIMAL_KHR
            | vk::Result::ERROR_OUT_OF_DATE_KHR
            | vk::Result::ERROR_SURFACE_LOST_KHR
            | vk::Result::ERROR_FULL_SCREEN_EXCLUSIVE_MODE_LOST_EXT,
        ) => PresentResultDisposition::Enqueued,
        Err(vk::Result::ERROR_OUT_OF_HOST_MEMORY | vk::Result::ERROR_OUT_OF_DEVICE_MEMORY) => {
            PresentResultDisposition::NotEnqueued
        }
        Err(vk::Result::ERROR_DEVICE_LOST) => PresentResultDisposition::Uncertain,
        Err(_) => PresentResultDisposition::Uncertain,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PRESENT_FENCE_CAPACITY, PresentFences, PresentResultDisposition, classify_present_result,
    };
    use ash::vk;

    #[test]
    fn cancelled_present_returns_its_fence_to_the_pool() {
        let mut fences = PresentFences::new();
        assert!(fences.add(7_u32));
        assert_eq!(fences.reserve(), Some(7));
        assert!(fences.finish(PresentResultDisposition::NotEnqueued, 0));
        assert_eq!(fences.reserve(), Some(7));
    }

    #[test]
    fn signaled_present_fence_releases_its_generation_and_can_be_reused() {
        let mut fences = PresentFences::new();
        assert!(fences.add(11_u32));
        assert_eq!(fences.reserve(), Some(11));
        assert!(fences.finish(PresentResultDisposition::Enqueued, 4));
        assert_eq!(fences.outstanding(4), 1);
        assert_eq!(fences.outstanding(5), 0);

        let completed = fences
            .poll(|fence| Ok::<_, ()>(fence == 11))
            .expect("poll succeeds");
        assert_eq!(completed, vec![(11, 4)]);
        assert_eq!(fences.outstanding(4), 0);
        assert_eq!(fences.reserve(), Some(11));
    }

    #[test]
    fn present_fence_pool_has_a_fixed_capacity() {
        let mut fences = PresentFences::new();
        for fence in 0..PRESENT_FENCE_CAPACITY {
            assert!(fences.add(fence));
        }
        assert!(!fences.add(PRESENT_FENCE_CAPACITY));
    }

    #[test]
    fn a_full_present_fence_pool_can_wait_for_its_oldest_generation() {
        let mut fences = PresentFences::new();
        for generation in 1..=PRESENT_FENCE_CAPACITY as u64 {
            let fence = generation as u32;
            assert!(fences.add(fence));
            assert_eq!(fences.reserve(), Some(fence));
            assert!(fences.finish(PresentResultDisposition::Enqueued, generation));
        }

        assert_eq!(fences.reserve(), None);
        assert_eq!(fences.oldest_pending_generation(), Some(1));
        assert_eq!(fences.complete_generation(1), 1);
        assert_eq!(fences.reserve(), Some(1));
    }

    #[test]
    fn finishing_present_results_transitions_reserved_fences() {
        let mut fences = PresentFences::new();
        for fence in 1..=3 {
            assert!(fences.add(fence));
        }

        assert_eq!(fences.reserve(), Some(1));
        assert!(fences.finish(PresentResultDisposition::Enqueued, 4));
        assert_eq!(fences.reserve(), Some(2));
        assert!(fences.finish(PresentResultDisposition::NotEnqueued, 5));
        assert_eq!(fences.reserve(), Some(2));
        assert!(fences.finish(PresentResultDisposition::Uncertain, 6));

        assert_eq!(fences.outstanding(4), 1);
        assert_eq!(fences.outstanding(5), 0);
        assert_eq!(fences.outstanding(6), 1);
        assert_eq!(fences.waiting_fences(), vec![1, 2]);
    }

    #[test]
    fn complete_generation_releases_only_pending_and_uncertain_fences_for_that_generation() {
        let mut fences = PresentFences::new();
        for fence in 1..=2 {
            assert!(fences.add(fence));
            assert_eq!(fences.reserve(), Some(fence));
            let disposition = if fence == 1 {
                PresentResultDisposition::Enqueued
            } else {
                PresentResultDisposition::Uncertain
            };
            assert!(fences.finish(disposition, fence as u64));
        }

        assert_eq!(fences.complete_generation(1), 1);
        assert_eq!(fences.outstanding(1), 0);
        assert_eq!(fences.outstanding(2), 1);
    }

    #[test]
    fn waiting_fences_omits_reserved_fences() {
        let mut fences = PresentFences::new();
        assert!(fences.add(1_u32));
        assert!(fences.add(2_u32));
        assert_eq!(fences.reserve(), Some(1));
        assert!(fences.finish(PresentResultDisposition::Enqueued, 9));
        assert_eq!(fences.reserve(), Some(2));

        assert_eq!(fences.waiting_fences(), vec![1]);
    }

    #[test]
    fn present_results_distinguish_queued_unqueued_and_uncertain_operations() {
        for result in [
            Ok(false),
            Ok(true),
            Err(vk::Result::SUBOPTIMAL_KHR),
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR),
            Err(vk::Result::ERROR_SURFACE_LOST_KHR),
            Err(vk::Result::ERROR_FULL_SCREEN_EXCLUSIVE_MODE_LOST_EXT),
        ] {
            assert_eq!(
                classify_present_result(result),
                PresentResultDisposition::Enqueued
            );
        }
        for result in [
            Err(vk::Result::ERROR_OUT_OF_HOST_MEMORY),
            Err(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY),
        ] {
            assert_eq!(
                classify_present_result(result),
                PresentResultDisposition::NotEnqueued
            );
        }
        assert_eq!(
            classify_present_result(Err(vk::Result::ERROR_DEVICE_LOST)),
            PresentResultDisposition::Uncertain
        );
        assert_eq!(
            classify_present_result(Err(vk::Result::ERROR_UNKNOWN)),
            PresentResultDisposition::Uncertain
        );
    }

    #[test]
    fn classified_present_result_is_applied_to_the_reserved_fence() {
        for (result, expected_outstanding) in [
            (Ok(false), 1),
            (Err(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY), 0),
            (Err(vk::Result::ERROR_DEVICE_LOST), 1),
        ] {
            let mut fences = PresentFences::new();
            assert!(fences.add(1_u32));
            assert_eq!(fences.reserve(), Some(1));
            let disposition = classify_present_result(result);
            assert!(fences.finish(disposition, 3));
            assert_eq!(fences.outstanding(3), expected_outstanding);
            assert_eq!(
                fences.waiting_fences(),
                if expected_outstanding == 0 {
                    vec![]
                } else {
                    vec![1]
                }
            );
        }
    }
}

use super::SceneGraph;
use crate::types::*;

impl SceneGraph {
    // ─── Resource registry ────────────────────────────────────────────────

    /// Register a resource as available for use in [`NodeData::StaticImage`] nodes.
    ///
    /// Must be called after a successful resource upload before any agent-submitted
    /// [`crate::mutation::SceneMutation::AddNode`] or
    /// [`crate::mutation::SceneMutation::SetTileRoot`] referencing the resource.
    /// Spec: resource-store/spec.md §Requirement: Resource Upload Before Tile Creation.
    ///
    /// Calling this for an already-registered resource is a no-op: the existing
    /// entry (with its current node ref count) is preserved.
    pub fn register_resource(&mut self, id: ResourceId) {
        self.registered_resources.entry(id).or_insert(0);
    }

    /// Returns `true` if the resource has been registered (uploaded).
    pub fn is_resource_registered(&self, id: &ResourceId) -> bool {
        self.registered_resources.contains_key(id)
    }

    /// Returns the current node reference count for a resource, or `None` if the
    /// resource has not been registered.
    #[cfg(test)]
    pub(crate) fn resource_ref_count(&self, id: &ResourceId) -> Option<u32> {
        self.registered_resources.get(id).copied()
    }

    /// Increment the ref count for a resource that is referenced by a scene node.
    ///
    /// Only called internally when a `StaticImageNode` is inserted into the scene.
    /// Panics in debug builds if the resource has not been registered via
    /// [`register_resource`] first, since incrementing an unknown resource
    /// would silently bootstrap a registry entry and undermine the
    /// upload-before-use invariant.
    pub(super) fn inc_resource_ref(&mut self, id: ResourceId) {
        if let Some(count) = self.registered_resources.get_mut(&id) {
            *count += 1;
        } else {
            debug_assert!(
                false,
                "attempted to increment ref count for unregistered resource: {id:?}"
            );
        }
    }

    /// Decrement the ref count for a resource.  When the count reaches zero the
    /// resource is removed from the registry entirely (freeing it).
    ///
    /// Only called internally when a `StaticImageNode` is removed from the scene.
    pub(super) fn dec_resource_ref(&mut self, id: &ResourceId) {
        if let Some(count) = self.registered_resources.get_mut(id) {
            if *count <= 1 {
                self.registered_resources.remove(id);
            } else {
                *count -= 1;
            }
        }
    }
}

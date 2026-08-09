use crate::descriptor::{CapabilityDescriptor, ToolRouterBinding};
use crate::exposure::ToolExposure;
use crate::id::CapabilityId;
use std::collections::HashMap;

pub trait ToolRouter {
    fn resolve(&self, canonical_name: &str) -> Option<&CapabilityDescriptor>;
    fn list_model_visible(&self) -> Vec<&CapabilityDescriptor>;
    fn contains(&self, id: &CapabilityId) -> bool;
}

pub struct DefaultToolRouter {
    by_name: HashMap<String, CapabilityDescriptor>,
    by_id: HashMap<CapabilityId, CapabilityDescriptor>,
}

impl DefaultToolRouter {
    pub fn new(descriptors: impl IntoIterator<Item = CapabilityDescriptor>) -> Self {
        let mut by_name = HashMap::new();
        let mut by_id = HashMap::new();
        for d in descriptors.into_iter() {
            let key = d.id.canonical_name.clone();
            by_name.insert(key, d.clone());
            by_id.insert(d.id.clone(), d);
        }
        Self { by_name, by_id }
    }

    pub fn from_binding(
        binding: &ToolRouterBinding,
        all_descriptors: &HashMap<CapabilityId, CapabilityDescriptor>,
    ) -> Self {
        let mut by_name = HashMap::new();
        let mut by_id = HashMap::new();
        for id in &binding.tool_capability_ids {
            if let Some(desc) = all_descriptors.get(id).cloned() {
                by_name.insert(desc.id.canonical_name.clone(), desc.clone());
                by_id.insert(id.clone(), desc);
            }
        }
        Self { by_name, by_id }
    }
}

impl ToolRouter for DefaultToolRouter {
    fn resolve(&self, canonical_name: &str) -> Option<&CapabilityDescriptor> {
        self.by_name.get(canonical_name)
    }

    fn list_model_visible(&self) -> Vec<&CapabilityDescriptor> {
        self.by_name
            .values()
            .filter(|d| {
                matches!(
                    d.exposure,
                    ToolExposure::Direct | ToolExposure::Deferred | ToolExposure::CodeMode
                )
            })
            .collect()
    }

    fn contains(&self, id: &CapabilityId) -> bool {
        self.by_id.contains_key(id)
    }
}

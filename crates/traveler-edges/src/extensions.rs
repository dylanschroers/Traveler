//! Files that share a type. One pass over the interned extension column, so this is free.

use std::time::Instant;

use traveler_scan::{ExtId, FileIndex, Tree};

use crate::{EdgeKind, EdgeProvider, EdgeSet, EdgeStats, Group};

pub struct ExtensionGroups;

impl EdgeProvider for ExtensionGroups {
    fn kind(&self) -> EdgeKind {
        EdgeKind::Extension
    }

    fn compute(&self, index: &FileIndex) -> EdgeSet {
        let started = Instant::now();
        let n = index.ext_count();
        let mut members = vec![Vec::new(); n];
        let mut bytes = vec![0u64; n];

        for id in index.nodes() {
            if index.flags(id).is_dir() {
                continue;
            }
            let e = index.ext(id).0 as usize;
            members[e].push(id);
            bytes[e] += index.size(id);
        }

        let groups = members
            .into_iter()
            .enumerate()
            .filter(|(_, m)| m.len() >= 2)
            .map(|(e, m)| {
                let ext = index.ext_str(ExtId(e as u32));
                Group {
                    label: if ext.is_empty() {
                        "(no extension)".to_string()
                    } else {
                        format!(".{ext}")
                    },
                    bytes: bytes[e],
                    redundant_bytes: 0,
                    members: m,
                }
            })
            .collect();

        EdgeSet::finish(
            EdgeKind::Extension,
            groups,
            EdgeStats {
                millis: started.elapsed().as_millis() as u64,
                ..Default::default()
            },
        )
    }
}

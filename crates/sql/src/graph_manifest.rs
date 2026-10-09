//! Snapshot authority for the SQL adjacency layout. Physical routes belong to
//! the manifest, rather than following a later catalog segment rebuild.
use crate::graph_adjacency::GraphRecords;
use bicdb_catalog::ddl::{GraphPhysicalRoute, GraphPhysicalRoutes};
use bicdb_common::sha256::Sha256;
use bicdb_graph::{
    corpus_proof::{Root as CorpusRoot, Scope as CorpusScope},
    Error, Graph, GraphCorpus, Limits,
};
use std::collections::BTreeSet;

const MAGIC: &[u8; 8] = b"BICGRV3\0";
const LENGTH: usize = 124;
const MEASURED_LENGTH: usize = 148;
const PROOF_LENGTH: usize = 180;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CorpusStats {
    pub logical_bytes: u64,
    pub record_bytes: u64,
    pub generation: u64,
}
impl CorpusStats {
    pub fn measure(
        graph: &Graph,
        record_bytes: usize,
        generation: u64,
        limits: &Limits,
    ) -> Result<Self, Error> {
        Ok(Self {
            logical_bytes: graph.storage_size(limits.max_text_bytes)? as u64,
            record_bytes: record_bytes as u64,
            generation,
        })
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Manifest {
    pub ws: [u8; 8],
    pub file: u32,
    pub records: GraphRecords,
    pub routes: GraphPhysicalRoutes,
    pub next_id: u64,
    pub nodes: u64,
    pub edges: u64,
    pub stats: Option<CorpusStats>,
    /// Independent entity aggregate root. Absence means the source must use
    /// the established complete-validation compatibility path.
    pub proof: Option<[u8; 32]>,
}
fn fail(message: &str) -> Error {
    Error(message.into())
}
fn digest(data: &[u8]) -> [u8; 32] {
    let mut sha = Sha256::new();
    sha.update(data);
    sha.finalize()
}
impl Manifest {
    pub fn from_graph(
        ws: [u8; 8],
        file: u32,
        records: GraphRecords,
        routes: GraphPhysicalRoutes,
        graph: &Graph,
        stats: CorpusStats,
    ) -> Self {
        Self {
            ws,
            file,
            records,
            routes,
            next_id: graph.allocator_high_water(),
            nodes: graph.nodes().len() as u64,
            edges: graph.edges().len() as u64,
            stats: Some(stats),
            proof: None,
        }
    }
    fn validate(&self, ws: [u8; 8], file: u32, graph: u32, limits: &Limits) -> Result<(), Error> {
        let all = [
            self.records.heap,
            self.records.primary,
            self.routes.adjacency,
            self.routes.source,
            self.routes.locator,
            self.routes.incoming,
        ];
        if self.ws != ws
            || self.file != file
            || self.records.heap.obj != graph
            || self.routes.graph != graph
            || all.iter().any(|r| r.obj == 0 || r.block == 0)
            || all.iter().map(|r| r.obj).collect::<BTreeSet<_>>().len() != 6
            || all.iter().map(|r| r.block).collect::<BTreeSet<_>>().len() != 6
            || self.next_id == 0
            || self.next_id > 1 << 48
        {
            return Err(fail("invalid adjacency manifest scope/routes/allocator"));
        }
        if self.nodes > limits.max_nodes as u64 || self.edges > limits.max_edges as u64 {
            return Err(fail("graph element budget exceeded"));
        }
        if let Some(stats) = self.stats {
            let minimum = self
                .nodes
                .checked_add(self.edges)
                .and_then(|n| n.checked_mul(32))
                .and_then(|n| n.checked_add(self.logical_header().ok()?.len() as u64))
                .ok_or_else(|| fail("invalid adjacency corpus statistics"))?;
            if stats.generation == 0 || stats.logical_bytes == 0 || stats.record_bytes < minimum {
                return Err(fail("invalid adjacency corpus statistics"));
            }
            // Global source statistics must not consume a small point-read
            // request's byte budget. Full source/write validation applies it.
        }
        if self.proof.is_some() && self.stats.is_none() {
            return Err(fail("corpus proof requires measured manifest statistics"));
        }
        Ok(())
    }
    pub fn verify_corpus(
        &self,
        graph: &Graph,
        record_bytes: usize,
        limits: &Limits,
    ) -> Result<(), Error> {
        if let Some(stats) = self.stats {
            if CorpusStats::measure(graph, record_bytes, stats.generation, limits)? != stats {
                return Err(fail("adjacency corpus statistics differ from records"));
            }
        }
        Ok(())
    }
    pub fn advance(
        &mut self,
        graph: &Graph,
        patch: &bicdb_graph::storage::StoragePatch,
        limits: &Limits,
    ) -> Result<(), Error> {
        if patch.is_empty() && patch.entities_changed == 0 {
            return Ok(());
        }
        let generation = match self.stats {
            Some(stats) => stats
                .generation
                .checked_add(1)
                .ok_or_else(|| fail("adjacency generation exhausted"))?,
            None => 1,
        };
        let bytes = patch
            .after_record_bytes
            .ok_or_else(|| fail("missing final native corpus statistics"))?;
        self.stats = Some(if let Some(corpus) = patch.after_corpus {
            corpus.validate(limits)?;
            CorpusStats {
                logical_bytes: corpus.logical_bytes,
                record_bytes: corpus.record_bytes,
                generation,
            }
        } else {
            CorpusStats::measure(graph, bytes, generation, limits)?
        });
        // A caller must install the matching candidate root after updating all
        // entities. Never carry source proof authority across an unproved delta.
        self.proof = None;
        Ok(())
    }
    pub fn corpus(&self, limits: &Limits) -> Result<GraphCorpus, Error> {
        let stats = self
            .stats
            .ok_or_else(|| fail("corpus proof requires measured manifest statistics"))?;
        let corpus = GraphCorpus {
            next_id: self.next_id,
            nodes: self.nodes,
            edges: self.edges,
            logical_bytes: stats.logical_bytes,
            record_bytes: stats.record_bytes,
        };
        corpus.validate(limits)?;
        Ok(corpus)
    }
    pub fn proof_scope(&self) -> CorpusScope {
        let mut data = b"bicdb-native-graph-routes-v1".to_vec();
        data.extend(self.ws);
        data.extend(self.file.to_be_bytes());
        for route in [
            self.records.heap,
            self.records.primary,
            self.routes.adjacency,
            self.routes.source,
            self.routes.locator,
            self.routes.incoming,
        ] {
            data.extend(route.obj.to_be_bytes());
            data.extend(route.block.to_be_bytes());
        }
        CorpusScope {
            workspace: self.ws,
            graph: self.routes.graph,
            routes: digest(&data),
        }
    }
    pub fn install_proof(&mut self, root: CorpusRoot, limits: &Limits) -> Result<(), Error> {
        let stats = self
            .stats
            .ok_or_else(|| fail("corpus proof requires measured manifest statistics"))?;
        root.verify_manifest(
            self.proof_scope(),
            stats.generation,
            self.corpus(limits)?,
            limits,
        )?;
        self.proof = Some(root.hash);
        Ok(())
    }
    pub fn verify_proof(&self, root: CorpusRoot, limits: &Limits) -> Result<(), Error> {
        let expected = self
            .proof
            .ok_or_else(|| fail("manifest has no independent corpus proof"))?;
        if root.hash != expected {
            return Err(fail("corpus proof root differs from manifest"));
        }
        let stats = self.stats.expect("proof requires statistics");
        root.verify_manifest(
            self.proof_scope(),
            stats.generation,
            self.corpus(limits)?,
            limits,
        )
    }
    pub fn encode(&self, limits: &Limits) -> Result<Vec<u8>, Error> {
        self.validate(self.ws, self.file, self.routes.graph, limits)?;
        let mut data = MAGIC.to_vec();
        data.extend(self.ws);
        data.extend(self.file.to_be_bytes());
        for route in [
            self.records.heap,
            self.records.primary,
            self.routes.adjacency,
            self.routes.source,
            self.routes.locator,
            self.routes.incoming,
        ] {
            data.extend(route.obj.to_be_bytes());
            data.extend(route.block.to_be_bytes());
        }
        for n in [self.next_id, self.nodes, self.edges] {
            data.extend(n.to_be_bytes());
        }
        if let Some(stats) = self.stats {
            for n in [stats.logical_bytes, stats.record_bytes, stats.generation] {
                data.extend(n.to_be_bytes());
            }
        }
        if let Some(root) = self.proof {
            data.extend(root);
        }
        data.extend(digest(&data));
        debug_assert_eq!(
            data.len(),
            match (self.stats.is_some(), self.proof.is_some()) {
                (true, true) => PROOF_LENGTH,
                (true, false) => MEASURED_LENGTH,
                (false, false) => LENGTH,
                (false, true) => unreachable!("validated"),
            }
        );
        Ok(data)
    }
    pub fn decode(
        header: Option<&[u8]>,
        ws: [u8; 8],
        file: u32,
        graph: u32,
        limits: &Limits,
    ) -> Result<Option<Self>, Error> {
        let Some(data) = header else { return Ok(None) };
        if !data.starts_with(MAGIC) {
            return Ok(None);
        }
        if ![LENGTH, MEASURED_LENGTH, PROOF_LENGTH].contains(&data.len())
            || digest(&data[..data.len() - 32]) != data[data.len() - 32..]
        {
            return Err(fail("invalid adjacency manifest length/checksum"));
        }
        let u32_at = |i| u32::from_be_bytes(data[i..i + 4].try_into().expect("validated manifest"));
        let route = |i| GraphPhysicalRoute {
            obj: u32_at(i),
            block: u32_at(i + 4),
        };
        let u64_at = |i| u64::from_be_bytes(data[i..i + 8].try_into().expect("validated manifest"));
        let manifest = Self {
            ws: data[8..16].try_into().expect("workspace"),
            file: u32_at(16),
            records: GraphRecords {
                heap: route(20),
                primary: route(28),
            },
            routes: GraphPhysicalRoutes {
                graph: route(20).obj,
                adjacency: route(36),
                source: route(44),
                locator: route(52),
                incoming: route(60),
            },
            next_id: u64_at(68),
            nodes: u64_at(76),
            edges: u64_at(84),
            stats: (data.len() >= MEASURED_LENGTH).then(|| CorpusStats {
                logical_bytes: u64_at(92),
                record_bytes: u64_at(100),
                generation: u64_at(108),
            }),
            proof: (data.len() == PROOF_LENGTH)
                .then(|| data[116..148].try_into().expect("validated proof root")),
        };
        manifest.validate(ws, file, graph, limits)?;
        Ok(Some(manifest))
    }
    pub fn cache(&self, limits: &Limits) -> Result<Graph, Error> {
        bicdb_graph::storage::snapshot_cache(Some(&self.logical_header()?), limits)?
            .ok_or_else(|| fail("invalid adjacency logical manifest"))
    }
    pub fn validate_catalog(
        &self,
        cat: &mut bicdb_catalog::Catalog<'_>,
        engine: &bicdb_txn::engine::Engine<'_, '_, '_, '_>,
        view: bicdb_storage::cr::ReadView,
        limits: &Limits,
    ) -> Result<(), Error> {
        // The existing catalog API intentionally describes current committed
        // metadata. Keep its current-view path; historical manifests need CR.
        if view.snapshot.as_raw() >= cat.current_seq() {
            return self.validate_current_catalog(cat, view.snapshot);
        }
        use bicdb_catalog::{
            cache::{IcolRow, IndRow, ObjRow},
            dict,
        };
        engine.with_read_context(|pool, chain| {
            let mut remaining = limits.max_expansions;
            let key = bicdb_catalog::open::comp_num(u64::from(self.routes.graph));
            let object = cat
                .lookup_snapshot("i_obj_pk", &[Some(&key)], pool, chain, view, &mut remaining)
                .map_err(|e| Error(e.to_string()))?
                .ok_or_else(|| fail("missing adjacency graph at snapshot"))?;
            let object = ObjRow::from_values(&object.1).map_err(|e| Error(e.to_string()))?;
            if object.type_code != dict::obj_kind::GRAPH || object.status != 1 {
                return Err(fail("adjacency manifest requires a live graph"));
            }
            for (route, kind, descriptor) in [
                (
                    self.routes.adjacency,
                    dict::index_kind::GRAPH_ADJACENCY,
                    bicdb_catalog::ddl::graph_physical_descriptor(
                        dict::index_kind::GRAPH_ADJACENCY,
                    ),
                ),
                (
                    self.routes.source,
                    dict::index_kind::GRAPH_SOURCE_ENTRY,
                    bicdb_catalog::ddl::graph_physical_descriptor(
                        dict::index_kind::GRAPH_SOURCE_ENTRY,
                    ),
                ),
                (
                    self.routes.locator,
                    dict::index_kind::GRAPH_EDGE_LOCATOR,
                    bicdb_catalog::ddl::graph_physical_descriptor(
                        dict::index_kind::GRAPH_EDGE_LOCATOR,
                    ),
                ),
                (
                    self.routes.incoming,
                    dict::index_kind::GRAPH_REVERSE,
                    bicdb_catalog::ddl::graph_physical_descriptor(dict::index_kind::GRAPH_REVERSE),
                ),
                (self.records.primary, dict::index_kind::BTREE, None),
            ] {
                let key = bicdb_catalog::open::comp_num(u64::from(route.obj));
                let index = cat
                    .lookup_snapshot("i_ind_pk", &[Some(&key)], pool, chain, view, &mut remaining)
                    .map_err(|e| Error(e.to_string()))?
                    .ok_or_else(|| fail("adjacency manifest route belongs to another graph"))?;
                let index = IndRow::from_values(&index.1).map_err(|e| Error(e.to_string()))?;
                let position = bicdb_catalog::open::comp_num(1);
                let col = cat
                    .lookup_snapshot(
                        "i_icol_pk",
                        &[Some(&key), Some(&position)],
                        pool,
                        chain,
                        view,
                        &mut remaining,
                    )
                    .map_err(|e| Error(e.to_string()))?
                    .ok_or_else(|| fail("missing adjacency catalog key column"))?;
                let col = IcolRow::from_values(&col.1).map_err(|e| Error(e.to_string()))?;
                let extra = bicdb_catalog::open::comp_num(2);
                if cat
                    .lookup_snapshot(
                        "i_icol_pk",
                        &[Some(&key), Some(&extra)],
                        pool,
                        chain,
                        view,
                        &mut remaining,
                    )
                    .map_err(|e| Error(e.to_string()))?
                    .is_some()
                {
                    return Err(fail("invalid adjacency manifest extra key column"));
                }
                let primary = kind == dict::index_kind::BTREE;
                if index.bobj != self.routes.graph {
                    return Err(fail("adjacency manifest route belongs to another graph"));
                }
                if index.type_code != kind
                    || index.status != 1
                    || index.cols != 1
                    || index.expr_src.as_deref() != descriptor
                    || index.is_unique != primary
                    || col.col != u32::from(primary)
                    || col.pos != 1
                    || col.is_desc
                {
                    return Err(fail("invalid adjacency manifest catalog route"));
                }
            }
            Ok(())
        })
    }
    fn validate_current_catalog(
        &self,
        cat: &mut bicdb_catalog::Catalog<'_>,
        snapshot: bicdb_common::seq::CommitSeq,
    ) -> Result<(), Error> {
        use bicdb_catalog::dict;
        let object = cat
            .resolve_by_obj(snapshot, self.routes.graph)
            .map_err(|e| Error(e.to_string()))?;
        if object.type_code != dict::obj_kind::GRAPH || object.status != 1 {
            return Err(fail("adjacency manifest requires a live graph"));
        }
        let indexes = cat
            .indexes_of(snapshot, self.routes.graph)
            .map_err(|e| Error(e.to_string()))?;
        for (route, kind, descriptor) in [
            (
                self.routes.adjacency,
                dict::index_kind::GRAPH_ADJACENCY,
                bicdb_catalog::ddl::graph_physical_descriptor(dict::index_kind::GRAPH_ADJACENCY),
            ),
            (
                self.routes.source,
                dict::index_kind::GRAPH_SOURCE_ENTRY,
                bicdb_catalog::ddl::graph_physical_descriptor(dict::index_kind::GRAPH_SOURCE_ENTRY),
            ),
            (
                self.routes.locator,
                dict::index_kind::GRAPH_EDGE_LOCATOR,
                bicdb_catalog::ddl::graph_physical_descriptor(dict::index_kind::GRAPH_EDGE_LOCATOR),
            ),
            (
                self.routes.incoming,
                dict::index_kind::GRAPH_REVERSE,
                bicdb_catalog::ddl::graph_physical_descriptor(dict::index_kind::GRAPH_REVERSE),
            ),
            (self.records.primary, dict::index_kind::BTREE, None),
        ] {
            let Some(index) = indexes.iter().find(|index| index.obj == route.obj) else {
                return Err(fail("adjacency manifest route belongs to another graph"));
            };
            let primary = kind == dict::index_kind::BTREE;
            if index.kind != kind
                || index.status != 1
                || index.expr_src.as_deref() != descriptor
                || index.is_unique != primary
                || index.cols.len() != 1
                || index.cols[0].col != u32::from(primary)
                || index.cols[0].pos != 1
                || index.cols[0].is_desc
            {
                return Err(fail("invalid adjacency manifest catalog route"));
            }
        }
        Ok(())
    }
    pub fn logical_header(&self) -> Result<Vec<u8>, Error> {
        serde_json::to_vec(&serde_json::json!({"format":"bicdb-graph-records-v2",
            "next_id":self.next_id,"nodes":self.nodes,"edges":self.edges}))
        .map_err(|e| Error(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample() -> Manifest {
        let route = |obj| GraphPhysicalRoute {
            obj,
            block: obj + 10,
        };
        Manifest::from_graph(
            [7; 8],
            1,
            GraphRecords {
                heap: route(1),
                primary: route(2),
            },
            GraphPhysicalRoutes {
                graph: 1,
                adjacency: route(3),
                source: route(4),
                locator: route(5),
                incoming: route(6),
            },
            &Graph::new(),
            CorpusStats {
                logical_bytes: 65,
                record_bytes: 73,
                generation: 1,
            },
        )
    }
    #[test]
    fn measured_and_legacy_authorities_roundtrip_without_read_upgrade() {
        let limits = Limits::default();
        let manifest = sample();
        let bytes = manifest.encode(&limits).unwrap();
        assert_eq!(bytes.len(), MEASURED_LENGTH);
        assert_eq!(
            Manifest::decode(Some(&bytes), [7; 8], 1, 1, &limits)
                .unwrap()
                .unwrap()
                .stats,
            manifest.stats
        );
        let small = Limits {
            max_text_bytes: 1,
            ..limits.clone()
        };
        assert!(Manifest::decode(Some(&bytes), [7; 8], 1, 1, &small).is_ok());
        let mut legacy = manifest;
        legacy.stats = None;
        let bytes = legacy.encode(&limits).unwrap();
        assert_eq!(bytes.len(), LENGTH);
        let decoded = Manifest::decode(Some(&bytes), [7; 8], 1, 1, &limits)
            .unwrap()
            .unwrap();
        assert!(decoded.stats.is_none());
        assert_eq!(decoded.encode(&limits).unwrap(), bytes);
    }
    #[test]
    fn independent_proof_root_is_scope_generation_and_corpus_bound() {
        let limits = Limits::default();
        let mut manifest = sample();
        let image = bicdb_graph::corpus_proof::Image::build(
            manifest.proof_scope(),
            manifest.next_id,
            manifest.stats.unwrap().generation,
            [],
            &limits,
            bicdb_graph::Deadline::for_limits(&limits),
        )
        .unwrap();
        // Use the proof-derived exact empty corpus rather than the fixture's
        // deliberately approximate metrics.
        let corpus = image.root.corpus(&limits).unwrap();
        manifest.stats = Some(CorpusStats {
            logical_bytes: corpus.logical_bytes,
            record_bytes: corpus.record_bytes,
            generation: image.root.generation,
        });
        manifest.install_proof(image.root, &limits).unwrap();
        let bytes = manifest.encode(&limits).unwrap();
        assert_eq!(bytes.len(), PROOF_LENGTH);
        let decoded = Manifest::decode(Some(&bytes), [7; 8], 1, 1, &limits)
            .unwrap()
            .unwrap();
        decoded.verify_proof(image.root, &limits).unwrap();
        let mut wrong = image.root;
        wrong.generation += 1;
        assert!(decoded.verify_proof(wrong, &limits).is_err());
        let mut corrupt = bytes;
        corrupt[116] ^= 1;
        assert!(Manifest::decode(Some(&corrupt), [7; 8], 1, 1, &limits).is_err());
    }
    #[test]
    fn metrics_are_checksummed_bounded_and_checked_against_complete_source() {
        let limits = Limits::default();
        let manifest = sample();
        let mut bytes = manifest.encode(&limits).unwrap();
        bytes[100] ^= 1;
        assert!(Manifest::decode(Some(&bytes), [7; 8], 1, 1, &limits).is_err());
        for bad in [
            CorpusStats {
                generation: 0,
                ..manifest.stats.unwrap()
            },
            CorpusStats {
                record_bytes: 1,
                ..manifest.stats.unwrap()
            },
        ] {
            let mut corrupt = manifest;
            corrupt.stats = Some(bad);
            assert!(corrupt.encode(&limits).is_err());
        }
        assert!(manifest.verify_corpus(&Graph::new(), 73, &limits).is_err());
        let graph = Graph::new();
        let stats = CorpusStats::measure(&graph, 73, 1, &limits).unwrap();
        let mut valid = manifest;
        valid.stats = Some(stats);
        valid.verify_corpus(&graph, 73, &limits).unwrap();
        assert!(valid.verify_corpus(&graph, 74, &limits).is_err());
        let mut trailing = valid.encode(&limits).unwrap();
        trailing.push(0);
        assert!(Manifest::decode(Some(&trailing), [7; 8], 1, 1, &limits).is_err());
    }
    #[test]
    fn edge_only_generations_require_sizes_and_fail_closed_at_exhaustion() {
        let mut manifest = sample();
        let graph = Graph::new();
        let patch = bicdb_graph::storage::StoragePatch {
            entities_changed: 1,
            after_record_bytes: Some(73),
            ..Default::default()
        };
        assert!(patch.is_empty());
        manifest
            .advance(&graph, &patch, &Limits::default())
            .unwrap();
        assert_eq!(manifest.stats.unwrap().generation, 2);
        let unchanged = manifest.stats;
        manifest
            .advance(&graph, &Default::default(), &Limits::default())
            .unwrap();
        assert_eq!(manifest.stats, unchanged);
        manifest.stats.as_mut().unwrap().generation = u64::MAX;
        assert!(manifest
            .advance(&graph, &patch, &Limits::default())
            .unwrap_err()
            .to_string()
            .contains("exhausted"));
        assert_eq!(manifest.stats.unwrap().generation, u64::MAX);
        manifest.stats = None;
        assert!(manifest
            .advance(
                &graph,
                &bicdb_graph::storage::StoragePatch {
                    entities_changed: 1,
                    ..Default::default()
                },
                &Limits::default()
            )
            .is_err());
        manifest
            .advance(&graph, &patch, &Limits::default())
            .unwrap();
        assert_eq!(manifest.stats.unwrap().generation, 1);
    }
}

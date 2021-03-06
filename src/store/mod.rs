//! High-level API for working with terminus-store.
//!
//! It is expected that most users of this library will work exclusively with the types contained in this module.
pub mod sync;

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use crate::layer::{
    IdTriple, Layer, LayerBuilder, LayerCounts, LayerObjectLookup, LayerPredicateLookup,
    LayerSubjectLookup, ObjectLookup, ObjectType, PredicateLookup, StringTriple, SubjectLookup,
};
use crate::storage::directory::{DirectoryLabelStore, DirectoryLayerStore};
use crate::storage::memory::{MemoryLabelStore, MemoryLayerStore};
use crate::storage::{CachedLayerStore, LabelStore, LayerStore, LockingHashMapLayerCache};

use std::io;

use rayon;
use rayon::prelude::*;

/// A store, storing a set of layers and database labels pointing to these layers
#[derive(Clone)]
pub struct Store {
    label_store: Arc<dyn LabelStore>,
    layer_store: Arc<dyn LayerStore>,
}

/// A wrapper over a SimpleLayerBuilder, providing a thread-safe sharable interface
///
/// The SimpleLayerBuilder requires one to have a mutable reference to
/// it, and on commit it will be consumed. This builder only requires
/// an immutable reference, and uses a futures-aware read-write lock
/// to synchronize access to it between threads. Also, rather than
/// consuming itself on commit, this wrapper will simply mark itself
/// as having committed, returning errors on further calls.
pub struct StoreLayerBuilder {
    parent: Option<Arc<dyn Layer>>,
    builder: RwLock<Option<Box<dyn LayerBuilder>>>,
    name: [u32; 5],
    store: Store,
}

impl StoreLayerBuilder {
    async fn new(store: Store) -> io::Result<Self> {
        let builder = store.layer_store.create_base_layer().await?;

        Ok(Self {
            parent: builder.parent(),
            name: builder.name(),
            builder: RwLock::new(Some(builder)),
            store,
        })
    }

    fn wrap(builder: Box<dyn LayerBuilder>, store: Store) -> Self {
        StoreLayerBuilder {
            parent: builder.parent(),
            name: builder.name(),
            builder: RwLock::new(Some(builder)),
            store,
        }
    }

    fn with_builder<R, F: FnOnce(&mut Box<dyn LayerBuilder>) -> R>(
        &self,
        f: F,
    ) -> Result<R, io::Error> {
        let mut builder = self
            .builder
            .write()
            .expect("rwlock write should always succeed");
        match (*builder).as_mut() {
            None => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "builder has already been committed",
            )),
            Some(builder) => Ok(f(builder)),
        }
    }

    /// Returns the name of the layer being built
    pub fn name(&self) -> [u32; 5] {
        self.name
    }

    pub fn parent(&self) -> Option<Arc<dyn Layer>> {
        self.parent.clone()
    }

    /// Add a string triple
    pub fn add_string_triple(&self, triple: StringTriple) -> Result<(), io::Error> {
        self.with_builder(move |b| b.add_string_triple(triple))
    }

    /// Add an id triple
    pub fn add_id_triple(&self, triple: IdTriple) -> Result<(), io::Error> {
        self.with_builder(move |b| b.add_id_triple(triple))
    }

    /// Remove a string triple
    pub fn remove_string_triple(&self, triple: StringTriple) -> Result<(), io::Error> {
        self.with_builder(move |b| b.remove_string_triple(triple))
    }

    /// Remove an id triple
    pub fn remove_id_triple(&self, triple: IdTriple) -> Result<(), io::Error> {
        self.with_builder(move |b| b.remove_id_triple(triple))
    }

    /// Returns true if this layer has been committed, and false otherwise.
    pub fn committed(&self) -> bool {
        self.builder
            .read()
            .expect("rwlock write should always succeed")
            .is_none()
    }

    /// Commit the layer to storage without loading the resulting layer
    pub async fn commit_no_load(&self) -> io::Result<()> {
        let mut builder = None;
        {
            let mut guard = self
                .builder
                .write()
                .expect("rwlock write should always succeed");

            // Setting the builder to None ensures that committed() detects we already committed (or tried to do so anyway)
            std::mem::swap(&mut builder, &mut guard);
        }

        match builder {
            None => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "builder has already been committed",
            )),
            Some(builder) => builder.commit_boxed().await,
        }
    }

    /// Commit the layer to storage
    pub async fn commit(&self) -> io::Result<StoreLayer> {
        let name = self.name;
        self.commit_no_load().await?;

        let layer = self.store.layer_store.get_layer(name).await?;
        Ok(StoreLayer::wrap(
            layer.expect("layer that was just created was not found in store"),
            self.store.clone(),
        ))
    }

    pub fn apply_delta(&self, delta: &StoreLayer) -> Result<(), io::Error> {
        // create a child builder and use it directly
        // first check what dictionary entries we don't know about, add those
        rayon::join(
            || {
                delta.triple_additions().par_bridge().for_each(|t| {
                    delta
                        .id_triple_to_string(&t)
                        .map(|st| self.add_string_triple(st));
                });
            },
            || {
                delta.triple_removals().par_bridge().for_each(|t| {
                    delta
                        .id_triple_to_string(&t)
                        .map(|st| self.remove_string_triple(st));
                })
            },
        );

        Ok(())
    }

    pub fn apply_diff(&self, other: &StoreLayer) -> Result<(), io::Error> {
        // create a child builder and use it directly
        // first check what dictionary entries we don't know about, add those
        rayon::join(
            || {
                if let Some(this) = self.parent() {
                    this.triples().par_bridge().for_each(|t| {
                        this.id_triple_to_string(&t).map(|st| {
                            if !other.string_triple_exists(&st) {
                                self.remove_string_triple(st).unwrap()
                            };
                        });
                    })
                };
            },
            || {
                other.triples().par_bridge().for_each(|t| {
                    other.id_triple_to_string(&t).map(|st| {
                        if let Some(this) = self.parent() {
                            if !this.string_triple_exists(&st) {
                                self.add_string_triple(st).unwrap()
                            }
                        } else {
                            self.add_string_triple(st).unwrap()
                        };
                    });
                })
            },
        );

        Ok(())
    }
}

/// A layer that keeps track of the store it came out of, allowing the creation of a layer builder on top of this layer
#[derive(Clone)]
pub struct StoreLayer {
    // TODO this Arc here is not great
    layer: Arc<dyn Layer>,
    store: Store,
}

impl StoreLayer {
    fn wrap(layer: Arc<dyn Layer>, store: Store) -> Self {
        StoreLayer { layer, store }
    }

    /// Create a layer builder based on this layer
    pub async fn open_write(&self) -> io::Result<StoreLayerBuilder> {
        let layer = self
            .store
            .layer_store
            .create_child_layer(self.layer.name())
            .await?;

        Ok(StoreLayerBuilder::wrap(layer, self.store.clone()))
    }

    pub async fn parent(&self) -> io::Result<Option<StoreLayer>> {
        let parent_name = self.layer.parent_name();

        match parent_name {
            None => Ok(None),
            Some(parent_name) => match self.store.layer_store.get_layer(parent_name).await? {
                None => Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "parent layer not found even though it should exist",
                )),
                Some(layer) => Ok(Some(StoreLayer::wrap(layer, self.store.clone()))),
            },
        }
    }

    pub async fn squash(&self) -> io::Result<StoreLayer> {
        // TODO check if we already committed
        let new_builder = self.store.create_base_layer().await?;
        self.triples().par_bridge().for_each(|t| {
            let st = self.id_triple_to_string(&t).unwrap();
            new_builder.add_string_triple(st).unwrap()
        });

        new_builder.commit().await
    }
}

impl Layer for StoreLayer {
    fn name(&self) -> [u32; 5] {
        self.layer.name()
    }

    fn parent_name(&self) -> Option<[u32; 5]> {
        self.layer.parent_name()
    }

    fn node_and_value_count(&self) -> usize {
        self.layer.node_and_value_count()
    }

    fn predicate_count(&self) -> usize {
        self.layer.predicate_count()
    }

    fn subject_id(&self, subject: &str) -> Option<u64> {
        self.layer.subject_id(subject)
    }

    fn predicate_id(&self, predicate: &str) -> Option<u64> {
        self.layer.predicate_id(predicate)
    }

    fn object_node_id(&self, object: &str) -> Option<u64> {
        self.layer.object_node_id(object)
    }

    fn object_value_id(&self, object: &str) -> Option<u64> {
        self.layer.object_value_id(object)
    }

    fn id_subject(&self, id: u64) -> Option<String> {
        self.layer.id_subject(id)
    }

    fn id_predicate(&self, id: u64) -> Option<String> {
        self.layer.id_predicate(id)
    }

    fn id_object(&self, id: u64) -> Option<ObjectType> {
        self.layer.id_object(id)
    }

    fn subjects(&self) -> Box<dyn Iterator<Item = Box<dyn SubjectLookup>>> {
        self.layer.subjects()
    }

    fn subject_additions(&self) -> Box<dyn Iterator<Item = Box<dyn LayerSubjectLookup>>> {
        self.layer.subject_additions()
    }

    fn subject_removals(&self) -> Box<dyn Iterator<Item = Box<dyn LayerSubjectLookup>>> {
        self.layer.subject_removals()
    }

    fn lookup_subject(&self, subject: u64) -> Option<Box<dyn SubjectLookup>> {
        self.layer.lookup_subject(subject)
    }

    fn lookup_subject_addition(&self, subject: u64) -> Option<Box<dyn LayerSubjectLookup>> {
        self.layer.lookup_subject_addition(subject)
    }

    fn lookup_subject_removal(&self, subject: u64) -> Option<Box<dyn LayerSubjectLookup>> {
        self.layer.lookup_subject_removal(subject)
    }

    fn objects(&self) -> Box<dyn Iterator<Item = Box<dyn ObjectLookup>>> {
        self.layer.objects()
    }

    fn object_additions(&self) -> Box<dyn Iterator<Item = Box<dyn LayerObjectLookup>>> {
        self.layer.object_additions()
    }

    fn object_removals(&self) -> Box<dyn Iterator<Item = Box<dyn LayerObjectLookup>>> {
        self.layer.object_removals()
    }

    fn lookup_object(&self, object: u64) -> Option<Box<dyn ObjectLookup>> {
        self.layer.lookup_object(object)
    }

    fn lookup_object_addition(&self, object: u64) -> Option<Box<dyn LayerObjectLookup>> {
        self.layer.lookup_object_addition(object)
    }

    fn lookup_object_removal(&self, object: u64) -> Option<Box<dyn LayerObjectLookup>> {
        self.layer.lookup_object_removal(object)
    }

    fn predicates(&self) -> Box<dyn Iterator<Item = Box<dyn PredicateLookup>>> {
        self.layer.predicates()
    }

    fn predicate_additions(&self) -> Box<dyn Iterator<Item = Box<dyn LayerPredicateLookup>>> {
        self.layer.predicate_additions()
    }

    fn predicate_removals(&self) -> Box<dyn Iterator<Item = Box<dyn LayerPredicateLookup>>> {
        self.layer.predicate_removals()
    }

    fn lookup_predicate(&self, predicate: u64) -> Option<Box<dyn PredicateLookup>> {
        self.layer.lookup_predicate(predicate)
    }

    fn lookup_predicate_addition(&self, predicate: u64) -> Option<Box<dyn LayerPredicateLookup>> {
        self.layer.lookup_predicate_addition(predicate)
    }

    fn lookup_predicate_removal(&self, predicate: u64) -> Option<Box<dyn LayerPredicateLookup>> {
        self.layer.lookup_predicate_removal(predicate)
    }

    fn triple_exists(&self, subject: u64, predicate: u64, object: u64) -> bool {
        self.layer.triple_exists(subject, predicate, object)
    }

    fn triple_addition_exists(&self, subject: u64, predicate: u64, object: u64) -> bool {
        self.layer
            .triple_addition_exists(subject, predicate, object)
    }

    fn triple_removal_exists(&self, subject: u64, predicate: u64, object: u64) -> bool {
        self.layer.triple_removal_exists(subject, predicate, object)
    }

    fn triples(&self) -> Box<dyn Iterator<Item = IdTriple> + Send> {
        self.layer.triples()
    }

    fn triple_additions(&self) -> Box<dyn Iterator<Item = IdTriple> + Send> {
        self.layer.triple_additions()
    }

    fn triple_removals(&self) -> Box<dyn Iterator<Item = IdTriple> + Send> {
        self.layer.triple_removals()
    }

    fn triples_s(&self, subject: u64) -> Box<dyn Iterator<Item = IdTriple> + Send> {
        self.layer.triples_s(subject)
    }

    fn triple_additions_s(&self, subject: u64) -> Box<dyn Iterator<Item = IdTriple> + Send> {
        self.layer.triple_additions_s(subject)
    }

    fn triple_removals_s(&self, subject: u64) -> Box<dyn Iterator<Item = IdTriple> + Send> {
        self.layer.triple_removals_s(subject)
    }

    fn triples_sp(
        &self,
        subject: u64,
        predicate: u64,
    ) -> Box<dyn Iterator<Item = IdTriple> + Send> {
        self.layer.triples_sp(subject, predicate)
    }

    fn triple_additions_sp(
        &self,
        subject: u64,
        predicate: u64,
    ) -> Box<dyn Iterator<Item = IdTriple> + Send> {
        self.layer.triple_additions_sp(subject, predicate)
    }

    fn triple_removals_sp(
        &self,
        subject: u64,
        predicate: u64,
    ) -> Box<dyn Iterator<Item = IdTriple> + Send> {
        self.layer.triple_removals_sp(subject, predicate)
    }

    fn triples_p(&self, predicate: u64) -> Box<dyn Iterator<Item = IdTriple> + Send> {
        self.layer.triples_p(predicate)
    }

    fn triple_additions_p(&self, predicate: u64) -> Box<dyn Iterator<Item = IdTriple> + Send> {
        self.layer.triple_additions_p(predicate)
    }

    fn triple_removals_p(&self, predicate: u64) -> Box<dyn Iterator<Item = IdTriple> + Send> {
        self.layer.triple_removals_p(predicate)
    }

    fn triples_o(&self, object: u64) -> Box<dyn Iterator<Item = IdTriple> + Send> {
        self.layer.triples_o(object)
    }

    fn triple_additions_o(&self, object: u64) -> Box<dyn Iterator<Item = IdTriple> + Send> {
        self.layer.triple_additions_o(object)
    }

    fn triple_removals_o(&self, object: u64) -> Box<dyn Iterator<Item = IdTriple> + Send> {
        self.layer.triple_removals_o(object)
    }

    fn clone_boxed(&self) -> Box<dyn Layer> {
        Box::new(self.clone())
    }

    fn triple_layer_addition_count(&self) -> usize {
        self.layer.triple_layer_addition_count()
    }

    fn triple_layer_removal_count(&self) -> usize {
        self.layer.triple_layer_removal_count()
    }

    fn triple_addition_count(&self) -> usize {
        self.layer.triple_addition_count()
    }

    fn triple_removal_count(&self) -> usize {
        self.layer.triple_removal_count()
    }

    fn all_counts(&self) -> LayerCounts {
        self.layer.all_counts()
    }
}

/// A named graph in terminus-store.
///
/// Named graphs in terminus-store are basically just a label pointing
/// to a layer. Opening a read transaction to a named graph is just
/// getting hold of the layer it points at, as layers are
/// read-only. Writing to a named graph is just making it point to a
/// new layer.
pub struct NamedGraph {
    label: String,
    store: Store,
}

impl NamedGraph {
    fn new(label: String, store: Store) -> Self {
        NamedGraph { label, store }
    }

    pub fn name(&self) -> &str {
        &self.label
    }

    /// Returns the layer this database points at
    pub async fn head(&self) -> io::Result<Option<StoreLayer>> {
        let new_label = self.store.label_store.get_label(&self.label).await?;

        match new_label {
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "database not found",
            )),
            Some(new_label) => match new_label.layer {
                None => Ok(None),
                Some(layer) => {
                    let layer = self.store.layer_store.get_layer(layer).await?;
                    match layer {
                        None => Err(io::Error::new(
                            io::ErrorKind::NotFound,
                            "layer not found even though it is pointed at by a label",
                        )),
                        Some(layer) => Ok(Some(StoreLayer::wrap(layer, self.store.clone()))),
                    }
                }
            },
        }
    }

    /// Set the database label to the given layer if it is a valid ancestor, returning false otherwise
    pub async fn set_head(&self, layer: &StoreLayer) -> io::Result<bool> {
        let layer_name = layer.name();
        let label = self.store.label_store.get_label(&self.label).await?;
        if label.is_none() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "label not found"));
        }
        let label = label.unwrap();

        let set_is_ok = match label.layer {
            None => true,
            Some(retrieved_layer_name) => {
                self.store
                    .layer_store
                    .layer_is_ancestor_of(layer_name, retrieved_layer_name)
                    .await?
            }
        };

        if set_is_ok {
            self.store.label_store.set_label(&label, layer_name).await?;
        }

        Ok(set_is_ok)
    }

    /// Set the database label to the given layer if it is a valid ancestor, returning false otherwise
    pub async fn force_set_head(&self, layer: &StoreLayer) -> io::Result<bool> {
        let layer_name = layer.name();
        let label = self.store.label_store.get_label(&self.label).await?;
        match label {
            None => Err(io::Error::new(io::ErrorKind::NotFound, "label not found")),
            Some(label) => {
                self.store.label_store.set_label(&label, layer_name).await?;

                Ok(true)
            }
        }
    }
}

impl Store {
    /// Create a new store from the given label and layer store
    pub fn new<Labels: 'static + LabelStore, Layers: 'static + LayerStore>(
        label_store: Labels,
        layer_store: Layers,
    ) -> Store {
        Store {
            label_store: Arc::new(label_store),
            layer_store: Arc::new(layer_store),
        }
    }

    /// Create a new database with the given name
    ///
    /// If the database already exists, this will return an error
    pub async fn create(&self, label: &str) -> io::Result<NamedGraph> {
        let label = self.label_store.create_label(label).await?;
        Ok(NamedGraph::new(label.name, self.clone()))
    }

    /// Open an existing database with the given name, or None if it does not exist
    pub async fn open(&self, label: &str) -> io::Result<Option<NamedGraph>> {
        let label = self.label_store.get_label(label).await?;
        Ok(label.map(|label| NamedGraph::new(label.name, self.clone())))
    }

    pub async fn get_layer_from_id(&self, layer: [u32; 5]) -> io::Result<Option<StoreLayer>> {
        let layer = self.layer_store.get_layer(layer).await?;
        Ok(layer.map(|layer| StoreLayer::wrap(layer, self.clone())))
    }

    /// Create a base layer builder, unattached to any database label
    ///
    /// After having committed it, use `set_head` on a `NamedGraph` to attach it.
    pub async fn create_base_layer(&self) -> io::Result<StoreLayerBuilder> {
        StoreLayerBuilder::new(self.clone()).await
    }

    pub fn export_layers(&self, layer_ids: Box<dyn Iterator<Item = [u32; 5]>>) -> Vec<u8> {
        self.layer_store.export_layers(layer_ids)
    }
    pub fn import_layers(
        &self,
        pack: &[u8],
        layer_ids: Box<dyn Iterator<Item = [u32; 5]>>,
    ) -> Result<(), io::Error> {
        self.layer_store.import_layers(pack, layer_ids)
    }
}

/// Open a store that is entirely in memory
///
/// This is useful for testing purposes, or if the database is only going to be used for caching purposes
pub fn open_memory_store() -> Store {
    Store::new(
        MemoryLabelStore::new(),
        CachedLayerStore::new(MemoryLayerStore::new(), LockingHashMapLayerCache::new()),
    )
}

/// Open a store that stores its data in the given directory
pub fn open_directory_store<P: Into<PathBuf>>(path: P) -> Store {
    let p = path.into();
    Store::new(
        DirectoryLabelStore::new(p.clone()),
        CachedLayerStore::new(DirectoryLayerStore::new(p), LockingHashMapLayerCache::new()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use tokio::runtime::Runtime;

    fn create_and_manipulate_database(mut runtime: Runtime, store: Store) {
        let database = runtime.block_on(store.create("foodb")).unwrap();

        let head = runtime.block_on(database.head()).unwrap();
        assert!(head.is_none());

        let mut builder = runtime.block_on(store.create_base_layer()).unwrap();
        builder
            .add_string_triple(StringTriple::new_value("cow", "says", "moo"))
            .unwrap();

        let layer = runtime.block_on(builder.commit()).unwrap();
        assert!(runtime.block_on(database.set_head(&layer)).unwrap());

        builder = runtime.block_on(layer.open_write()).unwrap();
        builder
            .add_string_triple(StringTriple::new_value("pig", "says", "oink"))
            .unwrap();

        let layer2 = runtime.block_on(builder.commit()).unwrap();
        assert!(runtime.block_on(database.set_head(&layer2)).unwrap());
        let layer2_name = layer2.name();

        let layer = runtime.block_on(database.head()).unwrap().unwrap();

        assert_eq!(layer2_name, layer.name());
        assert!(layer.string_triple_exists(&StringTriple::new_value("cow", "says", "moo")));
        assert!(layer.string_triple_exists(&StringTriple::new_value("pig", "says", "oink")));
    }

    #[test]
    fn create_and_manipulate_memory_database() {
        let runtime = Runtime::new().unwrap();
        let store = open_memory_store();

        create_and_manipulate_database(runtime, store);
    }

    #[test]
    fn create_and_manipulate_directory_database() {
        let runtime = Runtime::new().unwrap();
        let dir = tempdir().unwrap();
        let store = open_directory_store(dir.path());

        create_and_manipulate_database(runtime, store);
    }

    #[test]
    fn create_layer_and_retrieve_it_by_id() {
        let mut runtime = Runtime::new().unwrap();

        let store = open_memory_store();
        let builder = runtime.block_on(store.create_base_layer()).unwrap();
        builder
            .add_string_triple(StringTriple::new_value("cow", "says", "moo"))
            .unwrap();

        let layer = runtime.block_on(builder.commit()).unwrap();

        let id = layer.name();

        let layer2 = runtime
            .block_on(store.get_layer_from_id(id))
            .unwrap()
            .unwrap();

        assert!(layer2.string_triple_exists(&StringTriple::new_value("cow", "says", "moo")));
    }

    #[test]
    fn commit_builder_makes_builder_committed() {
        let mut runtime = Runtime::new().unwrap();

        let store = open_memory_store();
        let builder = runtime.block_on(store.create_base_layer()).unwrap();

        builder
            .add_string_triple(StringTriple::new_value("cow", "says", "moo"))
            .unwrap();

        assert!(!builder.committed());

        runtime.block_on(builder.commit_no_load()).unwrap();

        assert!(builder.committed());
    }

    #[test]
    fn hard_reset() {
        let mut runtime = Runtime::new().unwrap();

        let store = open_memory_store();
        let database = runtime.block_on(store.create("foodb")).unwrap();

        let builder1 = runtime.block_on(store.create_base_layer()).unwrap();
        builder1
            .add_string_triple(StringTriple::new_value("cow", "says", "moo"))
            .unwrap();

        let layer1 = runtime.block_on(builder1.commit()).unwrap();

        assert!(runtime.block_on(database.set_head(&layer1)).unwrap());

        let builder2 = runtime.block_on(store.create_base_layer()).unwrap();
        builder2
            .add_string_triple(StringTriple::new_value("duck", "says", "quack"))
            .unwrap();

        let layer2 = runtime.block_on(builder2.commit()).unwrap();

        assert!(runtime.block_on(database.force_set_head(&layer2)).unwrap());

        let new_layer = runtime.block_on(database.head()).unwrap().unwrap();

        assert!(new_layer.string_triple_exists(&StringTriple::new_value("duck", "says", "quack")));
        assert!(!new_layer.string_triple_exists(&StringTriple::new_value("cow", "says", "moo")));
    }

    #[test]
    fn create_two_layers_and_squash() {
        let mut runtime = Runtime::new().unwrap();

        let store = open_memory_store();
        let builder = runtime.block_on(store.create_base_layer()).unwrap();
        builder
            .add_string_triple(StringTriple::new_value("cow", "says", "moo"))
            .unwrap();

        let layer = runtime.block_on(builder.commit()).unwrap();

        let builder2 = runtime.block_on(layer.open_write()).unwrap();

        builder2
            .add_string_triple(StringTriple::new_value("dog", "says", "woof"))
            .unwrap();

        let layer2 = runtime.block_on(builder2.commit()).unwrap();

        let new = runtime.block_on(layer2.squash()).unwrap();

        assert!(new.string_triple_exists(&StringTriple::new_value("cow", "says", "moo")));
        assert!(new.string_triple_exists(&StringTriple::new_value("dog", "says", "woof")));
        assert!(runtime.block_on(new.parent()).unwrap().is_none());
    }

    #[test]
    fn apply_a_base_delta() {
        let mut runtime = Runtime::new().unwrap();

        let store = open_memory_store();
        let builder = runtime.block_on(store.create_base_layer()).unwrap();

        builder
            .add_string_triple(StringTriple::new_value("cow", "says", "moo"))
            .unwrap();

        let layer = runtime.block_on(builder.commit()).unwrap();

        let builder2 = runtime.block_on(layer.open_write()).unwrap();

        builder2
            .add_string_triple(StringTriple::new_value("dog", "says", "woof"))
            .unwrap();

        let layer2 = runtime.block_on(builder2.commit()).unwrap();

        let delta_builder_1 = runtime.block_on(store.create_base_layer()).unwrap();

        delta_builder_1
            .add_string_triple(StringTriple::new_value("dog", "says", "woof"))
            .unwrap();
        delta_builder_1
            .add_string_triple(StringTriple::new_value("cat", "says", "meow"))
            .unwrap();

        let delta_1 = runtime.block_on(delta_builder_1.commit()).unwrap();

        let delta_builder_2 = runtime.block_on(delta_1.open_write()).unwrap();

        delta_builder_2
            .add_string_triple(StringTriple::new_value("crow", "says", "caw"))
            .unwrap();
        delta_builder_2
            .remove_string_triple(StringTriple::new_value("cat", "says", "meow"))
            .unwrap();

        let delta = runtime.block_on(delta_builder_2.commit()).unwrap();

        let rebase_builder = runtime.block_on(layer2.open_write()).unwrap();

        let _ = rebase_builder.apply_delta(&delta).unwrap();

        let rebase_layer = runtime.block_on(rebase_builder.commit()).unwrap();

        assert!(rebase_layer.string_triple_exists(&StringTriple::new_value("cow", "says", "moo")));
        assert!(rebase_layer.string_triple_exists(&StringTriple::new_value("crow", "says", "caw")));
        assert!(rebase_layer.string_triple_exists(&StringTriple::new_value("dog", "says", "woof")));
        assert!(!rebase_layer.string_triple_exists(&StringTriple::new_value("cat", "says", "meow")));
    }
}

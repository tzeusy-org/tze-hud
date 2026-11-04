//! External behavior suites, retained at their original source paths.

#[path = "../batch_atomicity.rs"]
mod batch_atomicity;

#[path = "../fuzz_scene_graph.rs"]
mod fuzz_scene_graph;

#[path = "../hit_test.rs"]
mod hit_test;

#[path = "../proptest_invariants.rs"]
mod proptest_invariants;

#[path = "../session_lifecycle.rs"]
mod session_lifecycle;

#[path = "../snapshot.rs"]
mod snapshot;

#[path = "../zone_interaction.rs"]
mod zone_interaction;

#[path = "../zone_ontology.rs"]
mod zone_ontology;

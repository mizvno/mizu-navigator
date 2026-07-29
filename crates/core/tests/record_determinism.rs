//! Determinism guarantees for `Value::Record`.
//!
//! These live in an integration test rather than the `#[cfg(test)]` module in
//! `core::types` deliberately: they must exercise only the crate's *public*
//! surface, since that surface is what a differing architecture or a differing
//! field insertion order would be observed through.

use mizu_core::core::types::{DECIMAL_SCALE, Value, from_json, hash_field, to_json};

/// `Value::Int` is fixed-point, so a bare `Value::from(1i64)` is 1e-8, not 1.
/// Tests that assert on serialized text need the scaled representation.
fn num(n: i64) -> Value {
    Value::Int(n * DECIMAL_SCALE)
}

/// FNV-1a/32 test vectors from the reference implementation.
///
/// This is the whole point of the hasher swap: the values below are constants
/// of the algorithm, not of the machine. A hasher that mixes at the native
/// word size (`FxHasher`, `DefaultHasher`) would produce different numbers on
/// a 32-bit target, and since these hashes are baked into parsed field-access
/// nodes, the parsed program would differ per architecture. If this test ever
/// fails, cross-architecture reproducibility is gone.
#[test]
fn hash_field_matches_fnv1a_32_reference_vectors() {
    assert_eq!(hash_field(""), 0x811c_9dc5);
    assert_eq!(hash_field("a"), 0xe40c_292c);
    assert_eq!(hash_field("foobar"), 0xbf9c_f968);
}

/// The hash must depend on byte order, not just on the multiset of bytes —
/// otherwise every permutation of a key name collides and the `u32` prefilter
/// in `get_field` stops filtering anything.
#[test]
fn hash_field_is_order_sensitive() {
    assert_ne!(hash_field("ab"), hash_field("ba"));
    assert_ne!(hash_field("name"), hash_field("mane"));
}

fn record(pairs: Vec<(&str, Value)>) -> Value {
    Value::record_from_unsorted(pairs)
}

fn keys(val: &Value) -> Vec<String> {
    match val {
        Value::Record(fields) => fields.iter().map(|f| f.key.to_string()).collect(),
        other => panic!("expected a record, got {other:?}"),
    }
}

#[test]
fn record_fields_are_stored_in_lexicographic_order() {
    let val = record(vec![
        ("zeta", Value::from(1i64)),
        ("alpha", Value::from(2i64)),
        ("Mid", Value::from(3i64)),
        ("beta", Value::from(4i64)),
    ]);
    // Byte order, so uppercase sorts before lowercase — the point is that the
    // order is a stated function of the key, not of insertion or of a hash.
    assert_eq!(keys(&val), ["Mid", "alpha", "beta", "zeta"]);
}

/// Structural equality compares the two field slices pairwise, so it is only
/// correct if both sides are ordered the same way regardless of how they were
/// built. Under hash ordering this held only by accident of the hash function.
#[test]
fn records_are_equal_regardless_of_insertion_order() {
    let a = record(vec![
        ("gamma", Value::from("g")),
        ("alpha", Value::from("a")),
        ("beta", Value::from("b")),
    ]);
    let b = record(vec![
        ("beta", Value::from("b")),
        ("gamma", Value::from("g")),
        ("alpha", Value::from("a")),
    ]);
    assert_eq!(a, b);
    assert_eq!(keys(&a), keys(&b));
}

#[test]
fn records_differing_in_one_value_are_not_equal() {
    let a = record(vec![("alpha", Value::from("x")), ("beta", Value::from("y"))]);
    let b = record(vec![("alpha", Value::from("x")), ("beta", Value::from("z"))]);
    assert_ne!(a, b);
}

/// Serialized output must be byte-identical for equal records, whatever order
/// the fields were inserted in — this is what made the previous hash ordering
/// look pseudo-random on the wire.
#[test]
fn to_json_emits_keys_in_alphabetical_order() {
    let val = record(vec![("zulu", num(1)), ("alpha", num(2)), ("mike", num(3))]);
    let encoded = serde_json::to_string(&to_json(&val).unwrap()).unwrap();
    assert_eq!(encoded, r#"{"alpha":2,"mike":3,"zulu":1}"#);
}

#[test]
fn to_json_orders_nested_records_too() {
    let inner = record(vec![("y", num(2)), ("x", num(1))]);
    let outer = record(vec![("outer_z", num(9)), ("inner_a", inner)]);
    let encoded = serde_json::to_string(&to_json(&outer).unwrap()).unwrap();
    assert_eq!(encoded, r#"{"inner_a":{"x":1,"y":2},"outer_z":9}"#);
}

/// `from_json` must establish the same ordering invariant the in-memory
/// constructors do, so a record that arrives over the network compares equal
/// to the identical record built locally.
#[test]
fn from_json_orders_keys_and_round_trips_through_to_json() {
    let parsed = from_json(&serde_json::json!({
        "zulu": 1,
        "alpha": 2,
        "mike": {"delta": 4, "bravo": 3}
    }))
    .unwrap();

    assert_eq!(keys(&parsed), ["alpha", "mike", "zulu"]);

    let re_encoded = serde_json::to_string(&to_json(&parsed).unwrap()).unwrap();
    assert_eq!(
        re_encoded,
        r#"{"alpha":2,"mike":{"bravo":3,"delta":4},"zulu":1}"#
    );
    assert_eq!(from_json(&to_json(&parsed).unwrap()).unwrap(), parsed);
}

#[test]
fn from_json_record_equals_locally_built_record() {
    let parsed = from_json(&serde_json::json!({"b": "two", "a": "one"})).unwrap();
    let built = record(vec![("a", Value::from("one")), ("b", Value::from("two"))]);
    assert_eq!(parsed, built);
}

/// `Display` walks the slice in order as well, so it inherits determinism from
/// the same invariant.
#[test]
fn display_renders_fields_in_alphabetical_order() {
    let val = record(vec![("c", num(3)), ("a", num(1))]);
    assert_eq!(val.to_string(), "{a: 1, c: 3}");
}

#[test]
fn get_field_finds_every_key_in_a_record() {
    let val = record(vec![
        ("zulu", Value::from(1i64)),
        ("alpha", Value::from(2i64)),
        ("mike", Value::from(3i64)),
    ]);
    for (key, expected) in [("alpha", 2i64), ("mike", 3), ("zulu", 1)] {
        assert_eq!(
            val.get_field(hash_field(key), key),
            Some(&Value::from(expected)),
            "lookup of {key} failed"
        );
    }
    assert_eq!(val.get_field(hash_field("absent"), "absent"), None);
}

/// The lookup prefilters on the hash but decides on the key, so a caller that
/// passes a hash belonging to a different key must not be handed that other
/// key's value.
#[test]
fn get_field_requires_the_key_to_match_not_just_the_hash() {
    let val = record(vec![("alpha", Value::from(1i64)), ("beta", Value::from(2i64))]);
    assert_eq!(val.get_field(hash_field("alpha"), "beta"), None);
    assert_eq!(val.get_field(hash_field("beta"), "alpha"), None);
}

#[test]
fn get_field_on_a_non_record_is_none() {
    assert_eq!(Value::from(1i64).get_field(hash_field("a"), "a"), None);
    assert_eq!(Value::Null.get_field(hash_field("a"), "a"), None);
}

/// Ordering must hold past any small-size threshold, and lookup must stay
/// correct across the whole slice rather than only near its front.
#[test]
fn ordering_and_lookup_hold_for_larger_records() {
    let mut pairs: Vec<(String, Value)> = (0..64)
        .map(|i| (format!("key_{i:03}"), Value::from(i as i64)))
        .collect();
    pairs.reverse();

    let val = Value::record_from_unsorted(pairs);

    let observed = keys(&val);
    let mut expected = observed.clone();
    expected.sort();
    assert_eq!(observed, expected);

    for i in 0..64i64 {
        let key = format!("key_{i:03}");
        assert_eq!(
            val.get_field(hash_field(&key), &key),
            Some(&Value::from(i)),
            "lookup of {key} failed"
        );
    }
}

use agentkit_contracts::{
    javascript_number, parse_javascript_json, stringify_javascript_value, turn_fingerprint,
};
use serde_json::json;

#[test]
fn callback_numbers_preserve_overflow_rounding_underflow_and_signed_zero() {
    let value = parse_javascript_json(r#"{"positive":1e400,"negative":-1e400,"zero":-0,"decimalZero":-0.0,"underflow":1e-400,"round":9007199254740993}"#).unwrap();
    assert_eq!(javascript_number(&value["positive"]), Some(f64::INFINITY));
    assert_eq!(
        javascript_number(&value["negative"]),
        Some(f64::NEG_INFINITY)
    );
    for key in ["zero", "decimalZero"] {
        assert!(javascript_number(&value[key]).unwrap().is_sign_negative());
    }
    assert_eq!(javascript_number(&value["underflow"]), Some(0.0));
    assert_eq!(
        javascript_number(&value["round"]),
        Some(9_007_199_254_740_992.0)
    );
    assert!(value["positive"].is_number());
    assert_eq!(
        stringify_javascript_value(&value).unwrap(),
        r#"{"positive":null,"negative":null,"zero":0,"decimalZero":0,"underflow":0,"round":9007199254740992}"#
    );
}

#[test]
fn stringification_uses_javascript_number_notation_and_property_order() {
    let value = parse_javascript_json(
        r#"{"z":1.0,"10":1e21,"2":1e20,"01":1e-7,"4294967295":1e-6,"tail":[1e400,-1e400,-0]}"#,
    )
    .unwrap();
    assert_eq!(
        stringify_javascript_value(&value).unwrap(),
        r#"{"2":100000000000000000000,"10":1e+21,"z":1,"01":1e-7,"4294967295":0.000001,"tail":[null,null,0]}"#
    );
}

#[test]
fn negative_zero_lexical_preservation_never_rewrites_strings_or_invalid_json() {
    let value = parse_javascript_json(r#"["-0", "escaped\"-0", {"-0":-0}, -0]"#).unwrap();
    assert_eq!(value[0], "-0");
    assert_eq!(value[1], "escaped\"-0");
    assert!(
        javascript_number(&value[2]["-0"])
            .unwrap()
            .is_sign_negative()
    );
    for (input, expected) in [("1e-0", 1.0), ("-1e-0", -1.0), ("1E-0", 1.0)] {
        assert_eq!(
            javascript_number(&parse_javascript_json(input).unwrap()),
            Some(expected)
        );
    }
    for invalid in ["-01", "--0", "-0x", "[1e400,]", "NaN", "Infinity"] {
        assert!(parse_javascript_json(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn fingerprints_coerce_nonfinite_values_at_the_javascript_stringify_boundary() {
    let input = parse_javascript_json(r#"{"value":[1e400,-1e400],"zero":-0}"#).unwrap();
    assert_eq!(
        turn_fingerprint(&input),
        turn_fingerprint(&json!({"value":[null,null],"zero":0}))
    );
}

#[test]
fn private_serde_number_key_remains_an_ordinary_json_object() {
    for name in ["Number", "RawValue"] {
        let key = format!("$serde_json::private::{name}");
        let input = format!("{{\"{key}\":\"123\"}}");
        let value = parse_javascript_json(&input).unwrap();
        assert!(value.is_object());
        assert_eq!(value[&key], "123");
        assert_eq!(stringify_javascript_value(&value).unwrap(), input);
    }
}

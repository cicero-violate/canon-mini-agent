  Remaining gaps (explicitly not covered yet)

  - Unknown/unsupported action: We generate a correction prompt, but the feedback JSON does not currently include an explicit schema_diff entry like “unsupported action”. We can add that.
  - Invalid field types: e.g., cmd not a string, payload object with non-string fields, patch not a string. We don’t currently emit schema_diff for type mismatches.
  - Missing action field: We cover via “missing field: action” implicitly? Not yet explicitly tested.
  - Message payload type present but not object: We now emit payload must be object but haven’t added a harness case that checks that string.
  - Role casing for from/to with valid casing but unknown role: No explicit error if role value is not one of expected roles.
  - Unexpected extra fields: No warnings (by design).
  - Non-message actions with extra required fields missing: We cover missing cmd, path, patch, code, crate but not “empty string is invalid” cases for those fields.
  - Status/type invalid values: We only check mismatch; we don’t validate allowed enums per role/type.

  If you want full explicitness, I can:

  1. Add explicit schema_diff entries for unsupported action and missing action.
  2. Add type-mismatch checks (e.g., cmd not string, payload not object, patch not string).
  3. Add invalid enum checks for type/status for message actions.
  4. Add harness cases for each of the above so every pathway is covered.

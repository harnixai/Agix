//! `agix.json.validate`: JSON Schema through the workspace's `jsonschema`,
//! with every reference outside the schema refused rather than fetched.

use jsonschema::error::ValidationErrorKind;
use jsonschema::{ReferencingError, Retrieve, Uri};
use serde_json::{json, Value};

use crate::Failure;

const MAX_ERRORS: usize = 100;

struct NoRetrieval;

impl Retrieve for NoRetrieval {
    fn retrieve(
        &self,
        uri: &Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err(format!("{uri} is outside the schema").into())
    }
}

pub(crate) fn validate(schema: &Value, value: &Value) -> Result<Value, Failure> {
    let validator = jsonschema::options()
        .should_validate_formats(true)
        .with_retriever(NoRetrieval)
        .build(schema)
        .map_err(|e| match e.kind() {
            ValidationErrorKind::Referencing(ReferencingError::Unretrievable { uri, .. }) => {
                Failure::new(
                    "json.external_ref",
                    format!(
                        "$ref to {uri}: only references inside the schema are resolved; \
                         nothing is fetched or read"
                    ),
                )
            }
            _ => Failure::new("json.schema", format!("the schema is not valid: {e}")),
        })?;
    let errors: Vec<Value> = validator
        .iter_errors(value)
        .take(MAX_ERRORS)
        .map(|e| {
            json!({
                "instancePath": e.instance_path().as_str(),
                "schemaPath": e.schema_path().as_str(),
                "message": e.to_string(),
            })
        })
        .collect();
    Ok(json!({ "valid": errors.is_empty(), "errors": errors }))
}

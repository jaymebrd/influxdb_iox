//! Protobuf types for errors from the google standards and
//! conversions to `tonic::Status`
//!
//! # Status Responses
//!
//! gRPC defines a standard set of [Status Codes] to use for various purposes. In general this
//! combined with a textual error message are sufficient. The status code allows the client to
//! handle classes of errors programmatically, whilst the error message provides additional context
//! to the end user. This is the minimal error model supported by all implementations of gRPC.
//!
//! gRPC also has a concept of [Error Details]. These are just a list of `google.protobuf.Any`
//! that can be bundled with the status message. A standard set of [Error Payloads] are used
//! by Google APIs, and this is a convention IOx follows.
//!
//! As the encoding of these payloads is somewhat arcane, Rust types such as [`FieldViolation`],
//! [`AlreadyExists`], [`NotFound`], [`PreconditionViolation`] etc... are provided that can be
//! converted to a `tonic::Status` with `Into::into`.
//!
//! Unfortunately client support for details payloads is patchy. Therefore, whilst IOx does
//! provide these payloads, they should be viewed as an optional extension and not mandatory
//! functionality for a workable client implementation
//!
//! # Message Conversion
//!
//! Most of the logic within IOx is written in terms of types defined in the `data_types` crate,
//! conversions are then defined to/from `generated_types` types generated by prost.
//!
//! In addition to avoiding a dependency on hyper, tonic, etc... within these crates, this serves
//! to abstract away concerns such as API evolution, default values, etc...
//!
//! Unfortunately, writing this translation code is somewhat cumbersome and so this module
//! contains a collection of extension traits to improve the ergonomics of writing the potentially
//! fallible conversion from the protobuf representation to the corresponding `data_types` type
//!
//! Each type should implement the following:
//!
//! * `From<data_types::Type>` for `proto::Type`
//! * `TryFrom<proto::Type, Error=FieldViolation>` for `data_types::Type`
//!
//! Where [`FieldViolation`] allows context propagation about the problematic field within a
//! nested structure. A common error type is chosen because:
//!
//! * It integrates well with the expectations of gRPC and by extension tonic
//! * It reduces boilerplate code
//!
//! [Status Codes]: https://grpc.github.io/grpc/core/md_doc_statuscodes.html
//! [Error Details]: https://cloud.google.com/apis/design/errors#error_details
//! [Error Payloads]: https://cloud.google.com/apis/design/errors#error_payloads
//!

pub mod protobuf {
    pub use pbjson_types::*;
}

pub mod rpc {
    include!(concat!(env!("OUT_DIR"), "/google.rpc.rs"));
    include!(concat!(env!("OUT_DIR"), "/google.rpc.serde.rs"));
}

pub mod longrunning {
    include!(concat!(env!("OUT_DIR"), "/google.longrunning.rs"));
    include!(concat!(env!("OUT_DIR"), "/google.longrunning.serde.rs"));

    use crate::google::{FieldViolation, FieldViolationExt, OptionalField};
    use crate::influxdata::iox::management::v1::{OperationMetadata, OPERATION_METADATA};
    use prost::{bytes::Bytes, Message};
    use std::convert::TryFrom;

    impl Operation {
        /// Return the IOx operation `id`. This `id` can
        /// be passed to the various APIs in the
        /// operations client such as `influxdb_iox_client::operations::Client::wait_operation`;
        pub fn id(&self) -> usize {
            self.name
                .parse()
                .expect("Internal error: id returned from server was not an integer")
        }

        /// Decodes an IOx `OperationMetadata` metadata payload
        pub fn iox_metadata(&self) -> Result<OperationMetadata, FieldViolation> {
            let metadata = self.metadata.as_ref().unwrap_field("metadata")?;

            if !crate::protobuf_type_url_eq(&metadata.type_url, OPERATION_METADATA) {
                return Err(FieldViolation {
                    field: "metadata.type_url".to_string(),
                    description: "Unexpected field type".to_string(),
                });
            }

            Message::decode(Bytes::clone(&metadata.value)).scope("metadata.value")
        }
    }

    /// Groups together an `Operation` with a decoded `OperationMetadata`
    ///
    /// When serialized this will serialize the encoded Any field on `Operation` along
    /// with its decoded representation as `OperationMetadata`
    #[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
    pub struct IoxOperation {
        /// The `Operation` message returned from the API
        pub operation: Operation,
        /// The decoded `Operation::metadata` contained within `IoxOperation::operation`
        pub metadata: OperationMetadata,
    }

    impl TryFrom<Operation> for IoxOperation {
        type Error = FieldViolation;

        fn try_from(operation: Operation) -> Result<Self, Self::Error> {
            Ok(Self {
                metadata: operation.iox_metadata()?,
                operation,
            })
        }
    }
}

use self::protobuf::Any;
use observability_deps::tracing::error;
use prost::{bytes::BytesMut, Message};
use std::convert::TryInto;

// A newtype struct to provide conversion into tonic::Status
#[derive(Debug)]
struct EncodeError(prost::EncodeError);

impl From<EncodeError> for tonic::Status {
    fn from(error: EncodeError) -> Self {
        error!(error=%error.0, "failed to serialise error response details");
        tonic::Status::unknown(format!("failed to serialise server error: {}", error.0))
    }
}

impl From<prost::EncodeError> for EncodeError {
    fn from(e: prost::EncodeError) -> Self {
        Self(e)
    }
}

fn encode_status(code: tonic::Code, message: String, details: Any) -> tonic::Status {
    let mut buffer = BytesMut::new();

    let status = rpc::Status {
        code: code as i32,
        message: message.clone(),
        details: vec![details],
    };

    match status.encode(&mut buffer) {
        Ok(_) => tonic::Status::with_details(code, message, buffer.freeze()),
        Err(e) => EncodeError(e).into(),
    }
}

/// Returns an iterator over the [`protobuf::Any`] payloads in the provided [`tonic::Status`]
fn get_details(status: &tonic::Status) -> impl Iterator<Item = protobuf::Any> {
    rpc::Status::decode(status.details())
        .ok()
        .into_iter()
        .flat_map(|status| status.details)
}

/// Error returned if a request field has an invalid value. Includes
/// machinery to add parent field names for context -- thus it will
/// report `rules.write_timeout` than simply `write_timeout`.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct FieldViolation {
    pub field: String,
    pub description: String,
}

impl FieldViolation {
    pub fn required(field: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            description: "Field is required".to_string(),
        }
    }

    /// Re-scopes this error as the child of another field
    pub fn scope(self, field: impl Into<String>) -> Self {
        let field = if self.field.is_empty() {
            field.into()
        } else {
            [field.into(), self.field].join(".")
        };

        Self {
            field,
            description: self.description,
        }
    }
}

impl std::error::Error for FieldViolation {}

impl std::fmt::Display for FieldViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Violation for field \"{}\": {}",
            self.field, self.description
        )
    }
}

fn encode_bad_request(violation: Vec<FieldViolation>) -> Result<Any, EncodeError> {
    let mut buffer = BytesMut::new();

    rpc::BadRequest {
        field_violations: violation
            .into_iter()
            .map(|f| rpc::bad_request::FieldViolation {
                field: f.field,
                description: f.description,
            })
            .collect(),
    }
    .encode(&mut buffer)?;

    Ok(Any {
        type_url: "type.googleapis.com/google.rpc.BadRequest".to_string(),
        value: buffer.freeze(),
    })
}

impl From<FieldViolation> for tonic::Status {
    fn from(f: FieldViolation) -> Self {
        let message = f.to_string();

        match encode_bad_request(vec![f]) {
            Ok(details) => encode_status(tonic::Code::InvalidArgument, message, details),
            Err(e) => e.into(),
        }
    }
}

impl From<rpc::bad_request::FieldViolation> for FieldViolation {
    fn from(v: rpc::bad_request::FieldViolation) -> Self {
        Self {
            field: v.field,
            description: v.description,
        }
    }
}

/// Returns an iterator over the [`FieldViolation`] in the provided [`tonic::Status`]
pub fn decode_field_violation(status: &tonic::Status) -> impl Iterator<Item = FieldViolation> {
    get_details(status)
        .filter(|details| details.type_url == "type.googleapis.com/google.rpc.BadRequest")
        .flat_map(|details| rpc::BadRequest::decode(details.value).ok())
        .flat_map(|bad_request| bad_request.field_violations)
        .map(Into::into)
}

/// An internal error occurred, no context is provided to the client
///
/// Should be reserved for when a fundamental invariant of the system has been broken
#[derive(Debug, Default, Clone)]
pub struct InternalError {}

impl From<InternalError> for tonic::Status {
    fn from(_: InternalError) -> Self {
        tonic::Status::new(tonic::Code::Internal, "Internal Error")
    }
}

/// A resource type within [`AlreadyExists`] or [`NotFound`]
#[derive(Debug, Clone, PartialEq)]
pub enum ResourceType {
    Database,
    Table,
    Partition,
    Chunk,
    DatabaseUuid,
    Job,
    Router,
    ServerId,
    Unknown(String),
}

impl ResourceType {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Database => "database",
            Self::DatabaseUuid => "database_uuid",
            Self::Table => "table",
            Self::Partition => "partition",
            Self::Chunk => "chunk",
            Self::Job => "job",
            Self::Router => "router",
            Self::ServerId => "server_id",
            Self::Unknown(unknown) => unknown,
        }
    }
}

impl From<String> for ResourceType {
    fn from(s: String) -> Self {
        match s.as_str() {
            "database" => Self::Database,
            "database_uuid" => Self::DatabaseUuid,
            "table" => Self::Table,
            "partition" => Self::Partition,
            "chunk" => Self::Chunk,
            "job" => Self::Job,
            "router" => Self::Router,
            "server_id" => Self::ServerId,
            _ => Self::Unknown(s),
        }
    }
}

impl std::fmt::Display for ResourceType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_str().fmt(f)
    }
}

/// Returns an iterator over the [`rpc::ResourceInfo`] payloads in the provided [`tonic::Status`]
fn decode_resource_info(status: &tonic::Status) -> impl Iterator<Item = rpc::ResourceInfo> {
    get_details(status)
        .filter(|details| details.type_url == "type.googleapis.com/google.rpc.ResourceInfo")
        .flat_map(|details| rpc::ResourceInfo::decode(details.value).ok())
}

/// IOx returns [`AlreadyExists`] when it is unable to create the requested entity
/// as it already exists on the server
#[derive(Debug, Clone, PartialEq)]
pub struct AlreadyExists {
    pub resource_type: ResourceType,
    pub resource_name: String,
    pub owner: String,
    pub description: String,
}

impl AlreadyExists {
    pub fn new(resource_type: ResourceType, resource_name: String) -> Self {
        let description = format!(
            "Resource {}/{} already exists",
            resource_type, resource_name
        );

        Self {
            resource_type,
            resource_name,
            description,
            owner: Default::default(),
        }
    }
}

fn encode_resource_info(
    resource_type: String,
    resource_name: String,
    owner: String,
    description: String,
) -> Result<Any, EncodeError> {
    let mut buffer = BytesMut::new();

    rpc::ResourceInfo {
        resource_type,
        resource_name,
        owner,
        description,
    }
    .encode(&mut buffer)?;

    Ok(Any {
        type_url: "type.googleapis.com/google.rpc.ResourceInfo".to_string(),
        value: buffer.freeze(),
    })
}

impl From<AlreadyExists> for tonic::Status {
    fn from(exists: AlreadyExists) -> Self {
        match encode_resource_info(
            exists.resource_type.to_string(),
            exists.resource_name,
            exists.owner,
            exists.description.clone(),
        ) {
            Ok(details) => encode_status(tonic::Code::AlreadyExists, exists.description, details),
            Err(e) => e.into(),
        }
    }
}

impl From<rpc::ResourceInfo> for AlreadyExists {
    fn from(r: rpc::ResourceInfo) -> Self {
        Self {
            resource_type: r.resource_type.into(),
            resource_name: r.resource_name,
            owner: r.owner,
            description: r.description,
        }
    }
}

/// Returns an iterator over the [`AlreadyExists`] in the provided [`tonic::Status`]
pub fn decode_already_exists(status: &tonic::Status) -> impl Iterator<Item = AlreadyExists> {
    decode_resource_info(status).map(Into::into)
}

/// IOx returns [`NotFound`] when it is unable to perform an operation on a resource
/// because it doesn't exist on the server
#[derive(Debug, Clone, PartialEq)]
pub struct NotFound {
    pub resource_type: ResourceType,
    pub resource_name: String,
    pub owner: String,
    pub description: String,
}

impl NotFound {
    pub fn new(resource_type: ResourceType, resource_name: String) -> Self {
        let description = format!("Resource {}/{} not found", resource_type, resource_name);

        Self {
            resource_type,
            resource_name,
            description,
            owner: Default::default(),
        }
    }
}

impl From<NotFound> for tonic::Status {
    fn from(not_found: NotFound) -> Self {
        match encode_resource_info(
            not_found.resource_type.to_string(),
            not_found.resource_name,
            not_found.owner,
            not_found.description.clone(),
        ) {
            Ok(details) => encode_status(tonic::Code::NotFound, not_found.description, details),
            Err(e) => e.into(),
        }
    }
}

impl From<rpc::ResourceInfo> for NotFound {
    fn from(r: rpc::ResourceInfo) -> Self {
        Self {
            resource_type: r.resource_type.into(),
            resource_name: r.resource_name,
            owner: r.owner,
            description: r.description,
        }
    }
}

/// Returns an iterator over the [`NotFound`] in the provided [`tonic::Status`]
pub fn decode_not_found(status: &tonic::Status) -> impl Iterator<Item = NotFound> {
    decode_resource_info(status).map(Into::into)
}

/// A [`PreconditionViolation`] is returned by IOx when the system is in a state that
/// prevents performing the requested operation
#[derive(Debug, Clone, PartialEq)]
pub enum PreconditionViolation {
    /// Server ID not set
    ServerIdNotSet,
    /// Database is not mutable
    DatabaseImmutable,
    /// Server not in required state for operation
    ServerInvalidState(String),
    /// Database not in required state for operation
    DatabaseInvalidState(String),
    /// Partition not in required state for operation
    PartitionInvalidState(String),
    /// Chunk not in required state for operation
    ChunkInvalidState(String),
    /// Configuration is immutable
    RouterConfigImmutable,
    /// Configuration is immutable
    DatabaseConfigImmutable,
    /// An unknown precondition violation
    Unknown {
        category: String,
        subject: String,
        description: String,
    },
}

impl PreconditionViolation {
    fn description(&self) -> String {
        match self {
            Self::ServerIdNotSet => "server id must be set".to_string(),
            Self::DatabaseImmutable => "database must be mutable".to_string(),
            Self::ServerInvalidState(description) => description.clone(),
            Self::DatabaseInvalidState(description) => description.clone(),
            Self::PartitionInvalidState(description) => description.clone(),
            Self::ChunkInvalidState(description) => description.clone(),
            Self::RouterConfigImmutable => "router configuration is not mutable".to_string(),
            Self::DatabaseConfigImmutable => "database configuration is not mutable".to_string(),
            Self::Unknown { description, .. } => description.clone(),
        }
    }
}

impl From<PreconditionViolation> for rpc::precondition_failure::Violation {
    fn from(v: PreconditionViolation) -> Self {
        match v {
            PreconditionViolation::ServerIdNotSet => Self {
                r#type: "server_id".to_string(),
                subject: "influxdata.com/iox".to_string(),
                description: v.description(),
            },
            PreconditionViolation::ServerInvalidState(_) => Self {
                r#type: "state".to_string(),
                subject: "influxdata.com/iox".to_string(),
                description: v.description(),
            },
            PreconditionViolation::DatabaseImmutable => Self {
                r#type: "mutable".to_string(),
                subject: "influxdata.com/iox/database".to_string(),
                description: v.description(),
            },
            PreconditionViolation::DatabaseInvalidState(_) => Self {
                r#type: "state".to_string(),
                subject: "influxdata.com/iox/database".to_string(),
                description: v.description(),
            },
            PreconditionViolation::PartitionInvalidState(_) => Self {
                r#type: "state".to_string(),
                subject: "influxdata.com/iox/partition".to_string(),
                description: v.description(),
            },
            PreconditionViolation::ChunkInvalidState(_) => Self {
                r#type: "state".to_string(),
                subject: "influxdata.com/iox/chunk".to_string(),
                description: v.description(),
            },
            PreconditionViolation::RouterConfigImmutable => Self {
                r#type: "config".to_string(),
                subject: "influxdata.com/iox/router".to_string(),
                description: v.description(),
            },
            PreconditionViolation::DatabaseConfigImmutable => Self {
                r#type: "config".to_string(),
                subject: "influxdata.com/iox/database".to_string(),
                description: v.description(),
            },
            PreconditionViolation::Unknown {
                category,
                subject,
                description,
            } => Self {
                r#type: category,
                subject,
                description,
            },
        }
    }
}

impl From<rpc::precondition_failure::Violation> for PreconditionViolation {
    fn from(v: rpc::precondition_failure::Violation) -> Self {
        match (v.r#type.as_str(), v.subject.as_str()) {
            ("server_id", "influxdata.com/iox") => PreconditionViolation::ServerIdNotSet,
            ("state", "influxdata.com/iox") => {
                PreconditionViolation::ServerInvalidState(v.description)
            }
            ("mutable", "influxdata.com/iox/database") => PreconditionViolation::DatabaseImmutable,
            ("state", "influxdata.com/iox/database") => {
                PreconditionViolation::DatabaseInvalidState(v.description)
            }
            ("state", "influxdata.com/iox/partition") => {
                PreconditionViolation::PartitionInvalidState(v.description)
            }
            ("state", "influxdata.com/iox/chunk") => {
                PreconditionViolation::ChunkInvalidState(v.description)
            }
            ("config", "influxdata.com/iox/router") => PreconditionViolation::RouterConfigImmutable,
            ("config", "influxdata.com/iox/database") => PreconditionViolation::DatabaseConfigImmutable,
            _ => Self::Unknown {
                category: v.r#type,
                subject: v.subject,
                description: v.description,
            },
        }
    }
}

/// Returns an iterator over the [`PreconditionViolation`] in the provided [`tonic::Status`]
pub fn decode_precondition_violation(
    status: &tonic::Status,
) -> impl Iterator<Item = PreconditionViolation> {
    get_details(status)
        .filter(|details| details.type_url == "type.googleapis.com/google.rpc.PreconditionFailure")
        .flat_map(|details| rpc::PreconditionFailure::decode(details.value).ok())
        .flat_map(|failure| failure.violations)
        .map(Into::into)
}

fn encode_precondition_failure(violations: Vec<PreconditionViolation>) -> Result<Any, EncodeError> {
    let mut buffer = BytesMut::new();

    rpc::PreconditionFailure {
        violations: violations.into_iter().map(Into::into).collect(),
    }
    .encode(&mut buffer)?;

    Ok(Any {
        type_url: "type.googleapis.com/google.rpc.PreconditionFailure".to_string(),
        value: buffer.freeze(),
    })
}

impl From<PreconditionViolation> for tonic::Status {
    fn from(violation: PreconditionViolation) -> Self {
        let message = violation.description();
        match encode_precondition_failure(vec![violation]) {
            Ok(details) => encode_status(tonic::Code::FailedPrecondition, message, details),
            Err(e) => e.into(),
        }
    }
}

/// An extension trait that adds the ability to convert an error
/// that can be converted to a String to a FieldViolation
///
/// This is useful where a field has fallible `TryFrom` conversion logic, but which doesn't
/// use [`FieldViolation`] as its error type. [`FieldViolationExt::scope`] will format the
/// returned error and add the field name as context
///
pub trait FieldViolationExt {
    type Output;

    fn scope(self, field: &'static str) -> Result<Self::Output, FieldViolation>;
}

impl<T, E> FieldViolationExt for Result<T, E>
where
    E: ToString,
{
    type Output = T;

    fn scope(self, field: &'static str) -> Result<T, FieldViolation> {
        self.map_err(|e| FieldViolation {
            field: field.to_string(),
            description: e.to_string(),
        })
    }
}

#[derive(Debug, Default, Clone)]
pub struct QuotaFailure {
    pub subject: String,
    pub description: String,
}

impl From<QuotaFailure> for tonic::Status {
    fn from(quota_failure: QuotaFailure) -> Self {
        tonic::Status::new(
            tonic::Code::ResourceExhausted,
            format!("{}: {}", quota_failure.subject, quota_failure.description),
        )
    }
}

/// An extension trait that adds the method `field` to any type implementing
/// `TryInto<U, Error = FieldViolation>`
///
/// This is primarily used to define other extension traits but may be useful for:
///
/// * Conversion code for a oneof enumeration
/// * Converting from a scalar field to a custom Rust type
///
/// In a lot of cases, the type will be `Option<proto::Type>` or `Vec<proto::Type>`
/// in which case `FromOptionalField` or `FromRepeatedField` should be used instead
///
pub trait FromField<T> {
    fn field(self, field: impl Into<String>) -> Result<T, FieldViolation>;
}

impl<T, U> FromField<U> for T
where
    T: TryInto<U, Error = FieldViolation>,
{
    /// Try to convert type using TryInto calling [`FieldViolation::scope`]
    /// on any returned error
    fn field(self, field: impl Into<String>) -> Result<U, FieldViolation> {
        self.try_into().map_err(|e| e.scope(field))
    }
}

/// An extension trait that adds the methods `from_optional` and `from_required` to any
/// Option containing a type implementing `TryInto<U, Error = FieldViolation>`
///
/// This is useful for converting message-typed fields such as `Option<prost::Type>` to
/// `Option<data_types::Type>` and `data_types::Type` respectively
pub trait FromOptionalField<T> {
    /// Converts an optional protobuf field to an option of a different type
    ///
    /// Returns None if the option is None, otherwise calls [`FromField::field`]
    /// on the contained data, returning any error encountered
    fn optional(self, field: impl Into<String>) -> Result<Option<T>, FieldViolation>;

    /// Converts an optional protobuf field to a different type, returning an error if None
    ///
    /// Returns `FieldViolation::required` if None, otherwise calls [`FromField::field`]
    /// on the contained data, returning any error encountered
    fn required(self, field: impl Into<String>) -> Result<T, FieldViolation>;
}

impl<T, U> FromOptionalField<U> for Option<T>
where
    T: TryInto<U, Error = FieldViolation>,
{
    fn optional(self, field: impl Into<String>) -> Result<Option<U>, FieldViolation> {
        self.map(|t| t.field(field)).transpose()
    }

    fn required(self, field: impl Into<String>) -> Result<U, FieldViolation> {
        match self {
            None => Err(FieldViolation::required(field)),
            Some(t) => t.field(field),
        }
    }
}

/// An extension trait that adds the method `from_repeated` to any `Vec` of a type
/// implementing `TryInto<U, Error = FieldViolation>`
///
/// This is useful for converting message-typed repeated fields such as `Vec<prost::Type>`
/// to `Vec<data_types::Type>`
pub trait FromRepeatedField<T> {
    /// Converts to a `Vec<U>`, short-circuiting on the first error and
    /// returning a correctly scoped `FieldViolation` for where the error
    /// was encountered
    fn repeated(self, field: impl Into<String>) -> Result<T, FieldViolation>;
}

impl<T, U> FromRepeatedField<Vec<U>> for Vec<T>
where
    T: TryInto<U, Error = FieldViolation>,
{
    fn repeated(self, field: impl Into<String>) -> Result<Vec<U>, FieldViolation> {
        let res: Result<_, _> = self
            .into_iter()
            .enumerate()
            .map(|(i, t)| t.field(i.to_string()))
            .collect();

        res.map_err(|e| e.scope(field))
    }
}

/// An extension trait that adds the method `non_empty` to any `String`
///
/// This is useful where code wishes to require a non-empty string is specified
///
/// TODO: Replace with NonEmptyString type implementing TryFrom?
pub trait NonEmptyString {
    /// Returns a Ok if the String is not empty
    fn non_empty(self, field: impl Into<String>) -> Result<String, FieldViolation>;
}

impl NonEmptyString for String {
    fn non_empty(self, field: impl Into<Self>) -> Result<String, FieldViolation> {
        if self.is_empty() {
            return Err(FieldViolation::required(field));
        }
        Ok(self)
    }
}

/// An extension trait that adds the method `required` to any `Option<T>`
///
/// This is useful for field types:
///
/// * With infallible conversions (e.g. `field.required("field")?.into()`)
/// * With conversion logic not implemented using `TryFrom`
///
/// `FromOptionalField` should be preferred where applicable
pub trait OptionalField<T> {
    fn unwrap_field(self, field: impl Into<String>) -> Result<T, FieldViolation>;
}

impl<T> OptionalField<T> for Option<T> {
    fn unwrap_field(self, field: impl Into<String>) -> Result<T, FieldViolation> {
        self.ok_or_else(|| FieldViolation::required(field))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn test_error_roundtrip() {
        let violation = FieldViolation::required("foobar");
        let status = tonic::Status::from(violation.clone());
        let collected: Vec<_> = decode_field_violation(&status).collect();
        assert_eq!(collected, vec![violation]);

        let not_found = NotFound::new(ResourceType::Chunk, "chunky".to_string());
        let status = tonic::Status::from(not_found.clone());
        let collected: Vec<_> = decode_not_found(&status).collect();
        assert_eq!(collected, vec![not_found]);

        let already_exists = AlreadyExists::new(ResourceType::Database, "my database".to_string());
        let status = tonic::Status::from(already_exists.clone());
        let collected: Vec<_> = decode_already_exists(&status).collect();
        assert_eq!(collected, vec![already_exists]);

        let precondition = PreconditionViolation::PartitionInvalidState("mumbo".to_string());
        let status = tonic::Status::from(precondition.clone());
        let collected: Vec<_> = decode_precondition_violation(&status).collect();
        assert_eq!(collected, vec![precondition]);
    }

    #[test]
    fn test_multiple() {
        // Should allow encoding multiple violations
        let violations = vec![
            FieldViolation::required("fizbuz"),
            FieldViolation::required("bingo"),
        ];

        let encoded = encode_bad_request(violations.clone()).unwrap();
        let mut buffer = BytesMut::new();

        let code = tonic::Code::InvalidArgument;

        let status = rpc::Status {
            code: code as i32,
            message: "message".to_string(),
            details: vec![
                // Should ignore unrecognised details payloads
                protobuf::Any {
                    type_url: "my_magic/type".to_string(),
                    value: Bytes::from(&b"INVALID"[..]),
                },
                encoded,
            ],
        };

        status.encode(&mut buffer).unwrap();
        let status = tonic::Status::with_details(code, status.message, buffer.freeze());
        let collected: Vec<_> = decode_field_violation(&status).collect();
        assert_eq!(collected, violations);
    }
}

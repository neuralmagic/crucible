//! Python bindings for `crucible-kueue-client`. Arguments and results cross as JSON text; the
//! `crucible_kueue` package turns them into Python values and typed errors. Each call comes as an
//! awaitable for asyncio and as a blocking call, both on one Tokio runtime.

use crucible_kueue_client::{Client, Error, Token};
use pyo3::exceptions::{PyException, PyValueError};
use pyo3::prelude::*;
use pyo3_async_runtimes::tokio::{future_into_py, get_runtime};
use serde_json::{Map, Value, json};
use std::time::Duration;

pyo3::create_exception!(_native, NativeError, PyException);

/// The exception for `error`: its message, then a JSON object naming its kind and what the
/// server answered.
fn raise(error: Error) -> PyErr {
    let detail = match &error {
        Error::Tool {
            status,
            retryable,
            body,
            ..
        } => json!({"kind": "tool", "status": status, "retryable": retryable, "body": body}),
        Error::Stuck { body, .. } => json!({"kind": "stuck", "body": body}),
        Error::Failed { reason, body, .. } => {
            json!({"kind": "failed", "reason": reason, "body": body})
        }
        Error::WaitTimeout { body, .. } => json!({"kind": "wait_timeout", "body": body}),
        Error::Refused { status, body, .. } => {
            json!({"kind": "refused", "http_status": status, "body": body})
        }
        Error::Rpc { code, .. } => json!({"kind": "rpc", "code": code}),
        Error::NoServers(_) | Error::Servers(_) | Error::NotGranted { .. } => {
            json!({"kind": "not_granted"})
        }
        Error::Token { .. } => json!({"kind": "token"}),
        Error::Client(_) | Error::Http { .. } => json!({"kind": "transport"}),
        Error::Protocol { .. } => json!({"kind": "protocol"}),
    };
    NativeError::new_err((error.to_string(), detail.to_string()))
}

fn object(arguments: &str) -> PyResult<Map<String, Value>> {
    match serde_json::from_str(arguments) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(PyValueError::new_err("arguments must be a JSON object")),
        Err(e) => Err(PyValueError::new_err(format!(
            "arguments are not JSON: {e}"
        ))),
    }
}

fn timeout(seconds: f64) -> PyResult<Duration> {
    Duration::try_from_secs_f64(seconds).map_err(|e| PyValueError::new_err(format!("timeout: {e}")))
}

#[pyclass(frozen)]
struct NativeClient {
    client: Client,
}

#[pymethods]
impl NativeClient {
    /// The client for the server the engine granted this task under `server`.
    #[staticmethod]
    fn from_env(server: &str) -> PyResult<Self> {
        Client::from_env(server)
            .map(|client| Self { client })
            .map_err(raise)
    }

    /// A client for `url` with a fixed `token`, or one read from `token_file` for each request.
    #[new]
    #[pyo3(signature = (url, *, token=None, token_file=None))]
    fn new(url: &str, token: Option<String>, token_file: Option<String>) -> PyResult<Self> {
        let token = match (token, token_file) {
            (Some(token), None) => Token::Static(token),
            (None, Some(file)) => Token::File(file.into()),
            _ => {
                return Err(PyValueError::new_err(
                    "give exactly one of token and token_file",
                ));
            }
        };
        Client::new(url, token)
            .map(|client| Self { client })
            .map_err(raise)
    }

    fn call<'py>(
        &self,
        py: Python<'py>,
        tool: String,
        arguments: &str,
    ) -> PyResult<Bound<'py, PyAny>> {
        let arguments = object(arguments)?;
        let client = self.client.clone();
        future_into_py(py, async move {
            client
                .call(&tool, arguments)
                .await
                .map(|value| value.to_string())
                .map_err(raise)
        })
    }

    fn call_blocking(&self, py: Python<'_>, tool: &str, arguments: &str) -> PyResult<String> {
        let arguments = object(arguments)?;
        py.detach(|| get_runtime().block_on(self.client.call(tool, arguments)))
            .map(|value| value.to_string())
            .map_err(raise)
    }

    #[pyo3(signature = (kind, name, until, namespace, timeout_seconds))]
    fn wait<'py>(
        &self,
        py: Python<'py>,
        kind: String,
        name: String,
        until: String,
        namespace: Option<String>,
        timeout_seconds: f64,
    ) -> PyResult<Bound<'py, PyAny>> {
        let timeout = timeout(timeout_seconds)?;
        let client = self.client.clone();
        future_into_py(py, async move {
            client
                .wait(&kind, &name, &until, namespace.as_deref(), timeout)
                .await
                .map(|value| value.to_string())
                .map_err(raise)
        })
    }

    #[pyo3(signature = (kind, name, until, namespace, timeout_seconds))]
    fn wait_blocking(
        &self,
        py: Python<'_>,
        kind: &str,
        name: &str,
        until: &str,
        namespace: Option<&str>,
        timeout_seconds: f64,
    ) -> PyResult<String> {
        let timeout = timeout(timeout_seconds)?;
        py.detach(|| {
            get_runtime().block_on(self.client.wait(kind, name, until, namespace, timeout))
        })
        .map(|value| value.to_string())
        .map_err(raise)
    }
}

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<NativeClient>()?;
    m.add("NativeError", m.py().get_type::<NativeError>())?;
    Ok(())
}

use pyo3::prelude::*;

#[pymodule]
fn wser(_m: &Bound<'_, PyModule>) -> PyResult<()> {
    Ok(())
}

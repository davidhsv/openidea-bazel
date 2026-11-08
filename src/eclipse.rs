//! Writes the model as an Eclipse project that jdtls imports (`.project`, `.classpath`, JDT prefs).

use crate::types::JavaModel;

pub struct EclipseFiles {
    pub project: String,
    pub classpath: String,
    pub jdt_core_prefs: String,
}

pub fn emit(model: &JavaModel, project_name: &str) -> EclipseFiles {
    let _ = (model, project_name);
    unimplemented!("agent D")
}

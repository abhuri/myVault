use std::fmt;
use std::path::Path;

use uuid::Uuid;

use crate::{CoreError, Result, VaultPath};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrashArea {
    Staging,
    Items,
}

impl TrashArea {
    fn component(self) -> &'static str {
        match self {
            Self::Staging => "staging",
            Self::Items => "items",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrashEntryKind {
    Manifest,
    Payload,
}

impl TrashEntryKind {
    fn component(self) -> &'static str {
        match self {
            Self::Manifest => "manifest.json",
            Self::Payload => "payload",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TrashId(Uuid);

impl TrashId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Parses a canonical lowercase, hyphenated UUID.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidTrashPath`] for every non-canonical representation.
    pub fn parse(value: &str) -> Result<Self> {
        let id = Uuid::parse_str(value)
            .map_err(|_| CoreError::InvalidTrashPath(Path::new(value).to_owned()))?;
        if id.to_string() != value {
            return Err(CoreError::InvalidTrashPath(Path::new(value).to_owned()));
        }
        Ok(Self(id))
    }
}

impl Default for TrashId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for TrashId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// A validated path in `.trash/v1/{staging|items}/<uuid>/{manifest.json|payload}`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrashPath {
    path: VaultPath,
    area: TrashArea,
    id: TrashId,
    kind: TrashEntryKind,
}

impl TrashPath {
    /// Constructs the exact v1 trash layout.
    ///
    /// # Errors
    /// Returns an error only if the generated portable path violates the vault contract.
    pub fn new(area: TrashArea, id: TrashId, kind: TrashEntryKind) -> Result<Self> {
        let path = VaultPath::from_portable(format!(
            ".trash/v1/{}/{}/{}",
            area.component(),
            id,
            kind.component()
        ))?;
        Ok(Self {
            path,
            area,
            id,
            kind,
        })
    }

    /// Parses only the exact canonical v1 layout.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidTrashPath`] for aliases, extra components, or unknown kinds.
    pub fn from_portable(value: &str) -> Result<Self> {
        let path = VaultPath::from_portable(value)
            .map_err(|_| CoreError::InvalidTrashPath(Path::new(value).to_owned()))?;
        if path.as_str() != value {
            return Err(CoreError::InvalidTrashPath(Path::new(value).to_owned()));
        }
        let components = value.split('/').collect::<Vec<_>>();
        let [".trash", "v1", area, id, kind] = components.as_slice() else {
            return Err(CoreError::InvalidTrashPath(Path::new(value).to_owned()));
        };
        let area = match *area {
            "staging" => TrashArea::Staging,
            "items" => TrashArea::Items,
            _ => return Err(CoreError::InvalidTrashPath(Path::new(value).to_owned())),
        };
        let kind = match *kind {
            "manifest.json" => TrashEntryKind::Manifest,
            "payload" => TrashEntryKind::Payload,
            _ => return Err(CoreError::InvalidTrashPath(Path::new(value).to_owned())),
        };
        Ok(Self {
            path,
            area,
            id: TrashId::parse(id)?,
            kind,
        })
    }

    #[must_use]
    pub fn as_vault_path(&self) -> &VaultPath {
        &self.path
    }

    #[must_use]
    pub fn area(&self) -> TrashArea {
        self.area
    }

    #[must_use]
    pub fn id(&self) -> TrashId {
        self.id
    }

    #[must_use]
    pub fn kind(&self) -> TrashEntryKind {
        self.kind
    }
}

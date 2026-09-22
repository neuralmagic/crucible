//! `dto!`: a wire struct and its `From<row>` conversion declared once.
//!
//! A field written as `name: Type` copies `row.name`; one written as `name: Type = expr` uses
//! the expression, with the row bound to the name the header gives it:
//!
//! ```ignore
//! dto! {
//!     /// One kept-candidate PR.
//!     pub struct KeptPrDto: From<k: KeptPr> {
//!         pub issue: String,
//!         pub status: String = k.status.as_str().to_string(),
//!     }
//! }
//! ```
//!
//! Every DTO derives `Debug`, `Serialize` and `ToSchema`; add another `#[derive(..)]` above the
//! struct for anything more. Field attributes and docs pass through to the struct.
macro_rules! dto {
    (
        $(#[$m:meta])*
        $vis:vis struct $name:ident : From<$row:ident : $src:ty> {
            $( $(#[$fm:meta])* $fvis:vis $field:ident : $ty:ty $(= $e:expr)? ),* $(,)?
        }
    ) => {
        $(#[$m])*
        #[derive(Debug, ::serde::Serialize, ::utoipa::ToSchema)]
        $vis struct $name {
            $( $(#[$fm])* $fvis $field: $ty, )*
        }

        impl ::core::convert::From<$src> for $name {
            fn from($row: $src) -> Self {
                $name {
                    $( $field: dto!(@field $row.$field $(, $e)?), )*
                }
            }
        }
    };
    (@field $copy:expr) => { $copy };
    (@field $copy:expr, $e:expr) => { $e };
}

pub(crate) use dto;

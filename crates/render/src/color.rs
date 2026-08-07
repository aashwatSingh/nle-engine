//! Color management, per spec 4.5. Internal working space is fixed:
//! linear light, Rec.709 primaries, 16-bit half float on GPU. Every input is
//! converted into this on decode; output is converted to the delivery space
//! on export. Actual GPU conversion passes are M4 work — this module fixes
//! the working-space contract so nothing downstream has to guess it.

use media::{ColorPrimaries, TransferFunction};

pub const WORKING_SPACE_PRIMARIES: ColorPrimaries = ColorPrimaries::Rec709;
pub const WORKING_SPACE_TRANSFER: TransferFunction = TransferFunction::Linear;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliverySpace {
    Rec709,
    Srgb,
    Rec2020Hlg,
}

/// TODO(M4): implement the actual matrix/transfer-function math as GPU
/// shader passes. This trait exists now so the compositor's pipeline shape
/// (decode -> convert-to-working -> composite -> convert-to-delivery) is
/// fixed before any effect code is written against it.
pub trait ColorConverter {
    fn to_working_space(&self, primaries: ColorPrimaries, transfer: TransferFunction);
    fn to_delivery_space(&self, target: DeliverySpace);
}

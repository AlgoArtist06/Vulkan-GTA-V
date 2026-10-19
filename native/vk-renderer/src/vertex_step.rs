//! D3D input-slot stepping translated to native vertex fetch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InputStep { Vertex, Instance, ConstantInstance }

impl InputStep {
    pub(crate) fn new(class: u32, rate: u32) -> Result<Self, String> {
        match (class, rate) {
            (0, 0) => Ok(Self::Vertex),
            (1, 1) => Ok(Self::Instance),
            (1, 0) => Ok(Self::ConstantInstance),
            _ => Err(format!("unsupported input-slot class={class} rate={rate}")),
        }
    }
    pub(crate) fn stride(self, stride: u32, bound: bool) -> u64 {
        if !bound || self == Self::ConstantInstance { 0 } else { u64::from(stride) }
    }
    pub(crate) fn shift(self, start: u64, base: i32, first_instance: u32,
                        indexed: bool, fold_vertex: bool, fold_instance: bool) -> u64 {
        match self {
            Self::ConstantInstance => u64::from(first_instance),
            Self::Instance if fold_instance => u64::from(first_instance),
            Self::Vertex if fold_vertex => if indexed { base.max(0) as u64 } else { start },
            _ => 0,
        }
    }
}



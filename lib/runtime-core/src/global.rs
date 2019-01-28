use crate::{
    types::{GlobalDesc, Type, Value},
    vm,
};
use std::{cell::UnsafeCell, rc::Rc};

pub struct Global {
    desc: GlobalDesc,
    storage: Rc<UnsafeCell<vm::LocalGlobal>>,
}

impl Global {
    pub fn new(value: Value) -> Self {
        Self::_new(value, false)
    }

    pub fn new_mutable(value: Value) -> Self {
        Self::_new(value, true)
    }

    fn _new(value: Value, mutable: bool) -> Self {
        let desc = GlobalDesc {
            mutable,
            ty: value.ty(),
        };

        let local_global = vm::LocalGlobal {
            data: match value {
                Value::I32(x) => x as u64,
                Value::I64(x) => x as u64,
                Value::F32(x) => x.to_bits() as u64,
                Value::F64(x) => x.to_bits(),
            },
        };

        Self {
            desc,
            storage: Rc::new(UnsafeCell::new(local_global))
        }
    }

    pub fn description(&self) -> GlobalDesc {
        self.desc
    }

    pub fn set(&mut self, value: Value) {
        if self.desc.mutable {
            if self.desc.ty == value.ty() {
                let local_global = vm::LocalGlobal {
                    data: match value {
                        Value::I32(x) => x as u64,
                        Value::I64(x) => x as u64,
                        Value::F32(x) => x.to_bits() as u64,
                        Value::F64(x) => x.to_bits(),
                    },
                };
                unsafe {
                    (*self.storage.get()) = local_global;
                }
            } else {
                panic!("Wrong type for setting this global")
            }
        } else {
            panic!("Cannot modify global immutable by default")
        }
    }

    pub fn get(&self) -> Value {
        let data = unsafe { (*self.storage.get()).data };

        match self.desc.ty {
            Type::I32 => Value::I32(data as i32),
            Type::I64 => Value::I64(data as i64),
            Type::F32 => Value::F32(f32::from_bits(data as u32)),
            Type::F64 => Value::F64(f64::from_bits(data)),
        }
    }

    pub(crate) fn vm_local_global(&mut self) -> *mut vm::LocalGlobal {
        &mut *unsafe { &mut *self.storage.get() }
    }
}

impl Clone for Global {
    fn clone(&self) -> Self {
        Self {
            desc: self.desc,
            storage: Rc::clone(&self.storage),
        }
    }
}
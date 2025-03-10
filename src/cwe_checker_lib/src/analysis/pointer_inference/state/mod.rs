use super::object_list::AbstractObjectList;
use super::{Data, ValueDomain};
use crate::abstract_domain::*;
use crate::intermediate_representation::*;
use crate::prelude::*;
use crate::utils::binary::RuntimeMemoryImage;
use std::collections::{BTreeMap, BTreeSet};

mod access_handling;
mod id_manipulation;
mod value_specialization;

/// Contains all information known about the state of a program at a specific point of time.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
pub struct State {
    /// Maps a register variable to the data known about its content.
    /// A variable not contained in the map has value `Data::Top(..)`, i.e. nothing is known about its content.
    register: BTreeMap<Variable, Data>,
    /// The list of all known memory objects.
    pub memory: AbstractObjectList,
    /// The abstract identifier of the current stack frame.
    /// It points to the base of the stack frame, i.e. only negative offsets point into the current stack frame.
    pub stack_id: AbstractIdentifier,
    /// All known IDs of caller stack frames.
    /// Note that these IDs are named after the callsite,
    /// i.e. we can distinguish every callsite and for recursive functions the caller and current stack frames have different IDs.
    ///
    /// Writes to the current stack frame with offset >= 0 are written to *all* caller stack frames.
    /// Reads to the current stack frame with offset >= 0 are handled as merge-read from all caller stack frames.
    pub caller_stack_ids: BTreeSet<AbstractIdentifier>,
    /// All IDs of objects that are known to some caller.
    /// This is an overapproximation of all object IDs that may have been passed as parameters to the function.
    /// The corresponding objects are not allowed to be deleted (even if no pointer to them exists anymore)
    /// so that after returning from a call the caller can recover their modified contents
    /// and the callee does not accidentally delete this information if it loses all pointers to an object.
    ///
    /// Note that IDs that the callee should not have access to are not included here.
    /// For these IDs the caller can assume that the contents of the corresponding memory object were not accessed or modified by the call.
    pub ids_known_to_caller: BTreeSet<AbstractIdentifier>,
}

impl State {
    /// Create a new state that contains only one memory object corresponding to the stack.
    /// The stack offset will be set to zero.
    pub fn new(stack_register: &Variable, function_tid: Tid) -> State {
        let stack_id = AbstractIdentifier::new(
            function_tid,
            AbstractLocation::from_var(stack_register).unwrap(),
        );
        let mut register: BTreeMap<Variable, Data> = BTreeMap::new();
        register.insert(
            stack_register.clone(),
            Data::from_target(
                stack_id.clone(),
                Bitvector::zero(apint::BitWidth::from(stack_register.size)).into(),
            ),
        );
        State {
            register,
            memory: AbstractObjectList::from_stack_id(stack_id.clone(), stack_register.size),
            stack_id,
            caller_stack_ids: BTreeSet::new(),
            ids_known_to_caller: BTreeSet::new(),
        }
    }

    /// Create a new state that contains one memory object corresponding to the stack
    /// and one memory object for each provided parameter register.
    ///
    /// This function can be used to approximate states of entry points
    /// where the number and types of its parameters is unknown.
    /// Note that this may also cause analysis errors,
    /// e.g. if two parameters point to the same memory object instead of different ones.
    pub fn new_with_generic_parameter_objects(
        stack_register: &Variable,
        function_tid: Tid,
        params: &[Variable],
    ) -> State {
        let mut state = State::new(stack_register, function_tid.clone());
        for param in params {
            let param_id = AbstractIdentifier::new(
                function_tid.clone(),
                AbstractLocation::from_var(param).unwrap(),
            );
            state.memory.add_abstract_object(
                param_id.clone(),
                Bitvector::zero(stack_register.size.into()).into(),
                super::object::ObjectType::Heap,
                stack_register.size,
            );
            state.set_register(
                param,
                DataDomain::from_target(param_id, Bitvector::zero(param.size.into()).into()),
            )
        }
        state
    }

    /// Set the MIPS link register `t9` to the address of the callee TID.
    ///
    /// According to the System V ABI for MIPS the caller has to save the callee address in register `t9`
    /// on a function call to position-independent code.
    /// This function manually sets `t9` to the correct value
    /// to mitigate cases where `t9` could not be correctly computed due to previous analysis errors.
    ///
    /// Returns an error if the callee address could not be parsed (e.g. for `UNKNOWN` addresses).
    pub fn set_mips_link_register(
        &mut self,
        callee_tid: &Tid,
        generic_pointer_size: ByteSize,
    ) -> Result<(), Error> {
        let link_register = Variable {
            name: "t9".into(),
            size: generic_pointer_size,
            is_temp: false,
        };
        let address = Bitvector::from_u64(u64::from_str_radix(&callee_tid.address, 16)?)
            .into_resize_unsigned(generic_pointer_size);
        // FIXME: A better way would be to test whether the link register contains the correct value
        // and only fix and log cases where it doesn't contain the correct value.
        // Right now this is unfortunately the common case,
        // so logging every case would generate too many log messages.
        self.set_register(&link_register, address.into());
        Ok(())
    }

    /// Clear all non-callee-saved registers from the state.
    /// This automatically also removes all virtual registers.
    /// The parameter is a list of callee-saved register names.
    pub fn clear_non_callee_saved_register(&mut self, callee_saved_register: &[Variable]) {
        let register = callee_saved_register
            .iter()
            .filter_map(|var| {
                let value = self.get_register(var);
                if value.is_top() {
                    None
                } else {
                    Some((var.clone(), value))
                }
            })
            .collect();
        self.register = register;
    }

    /// Mark those parameter values of an extern function call, that are passed on the stack,
    /// as unknown data (since the function may modify them).
    pub fn clear_stack_parameter(
        &mut self,
        extern_call: &ExternSymbol,
        global_memory: &RuntimeMemoryImage,
    ) -> Result<(), Error> {
        let mut result_log = Ok(());
        for arg in &extern_call.parameters {
            match arg {
                Arg::Register { .. } => (),
                Arg::Stack { address, size, .. } => {
                    let data_top = Data::new_top(*size);
                    if let Err(err) = self.write_to_address(address, &data_top, global_memory) {
                        result_log = Err(err);
                    }
                }
            }
        }
        // We only return the last error encountered.
        result_log
    }

    /// Remove all objects that cannot longer be reached by any known pointer.
    /// This does not remove objects, where some caller may still know a pointer to the object.
    ///
    /// The function uses an underapproximation of all possible pointer targets contained in a memory object.
    /// This keeps the number of tracked objects reasonably small.
    pub fn remove_unreferenced_objects(&mut self) {
        // get all referenced IDs
        let mut referenced_ids = BTreeSet::new();
        for (_reg_name, data) in self.register.iter() {
            referenced_ids.extend(data.referenced_ids().cloned());
        }
        referenced_ids.insert(self.stack_id.clone());
        referenced_ids.append(&mut self.caller_stack_ids.clone());
        referenced_ids.append(&mut self.ids_known_to_caller.clone());
        referenced_ids = self.add_directly_reachable_ids_to_id_set(referenced_ids);
        // remove unreferenced objects
        self.memory.remove_unused_objects(&referenced_ids);
    }

    /// Merge the callee stack with the caller stack.
    ///
    /// This deletes the memory object corresponding to the callee_id
    /// and updates all other references pointing to the callee_id to point to the caller_id.
    /// The offset adjustment is handled as in `replace_abstract_id`.
    ///
    /// Note that right now the content of the callee memory object is *not* merged into the caller memory object.
    /// In general this is the correct behaviour
    /// as the content below the stack pointer should be considered uninitialized memory after returning to the caller.
    /// However, an aggressively optimizing compiler or an unknown calling convention may deviate from this.
    pub fn merge_callee_stack_to_caller_stack(
        &mut self,
        callee_id: &AbstractIdentifier,
        caller_id: &AbstractIdentifier,
        offset_adjustment: &ValueDomain,
    ) {
        self.memory.remove_object(callee_id);
        self.replace_abstract_id(callee_id, caller_id, offset_adjustment);
    }

    /// Mark a memory object as already freed (i.e. pointers to it are dangling).
    /// If the object cannot be identified uniquely, all possible targets are marked as having an unknown status.
    ///
    /// If this may cause double frees (i.e. the object in question may have been freed already),
    /// an error with the list of possibly already freed objects is returned.
    pub fn mark_mem_object_as_freed(
        &mut self,
        object_pointer: &Data,
    ) -> Result<(), Vec<(AbstractIdentifier, Error)>> {
        self.memory.mark_mem_object_as_freed(object_pointer)
    }

    /// Remove all virtual register from the state.
    /// This should only be done in cases where it is known that no virtual registers can be alive.
    ///
    /// Example: At the start of a basic block no virtual registers should be alive.
    pub fn remove_virtual_register(&mut self) {
        self.register = self
            .register
            .clone()
            .into_iter()
            .filter(|(register, _value)| !register.is_temp)
            .collect();
    }

    /// Add those objects from the `caller_state` to `self`, that are not known to `self`.
    ///
    /// Since self does not know these objects, we assume that the current function could not have accessed
    /// them in any way during execution.
    /// This means they are unchanged from the moment of the call until the return from the call,
    /// thus we can simply copy their object-state from the moment of the call.
    pub fn readd_caller_objects(&mut self, caller_state: &State) {
        self.memory.append_unknown_objects(&caller_state.memory);
    }

    /// Restore the content of callee-saved registers from the caller state
    /// with the exception of the stack register.
    ///
    /// This function does not check what the callee state currently contains in these registers.
    /// If the callee does not adhere to the given calling convention, this may introduce analysis errors!
    /// It will also mask cases
    /// where a callee-saved register was incorrectly modified (e.g. because of a bug in the callee).
    pub fn restore_callee_saved_register(
        &mut self,
        caller_state: &State,
        cconv: &CallingConvention,
        stack_register: &Variable,
    ) {
        for register in cconv
            .callee_saved_register
            .iter()
            .filter(|reg| *reg != stack_register)
        {
            self.set_register(register, caller_state.get_register(register));
        }
    }

    /// Remove all knowledge about the contents of callee-saved registers from the state.
    pub fn remove_callee_saved_register(&mut self, cconv: &CallingConvention) {
        for register in &cconv.callee_saved_register {
            self.register.remove(register);
        }
    }
}

impl AbstractDomain for State {
    /// Merge two states
    fn merge(&self, other: &Self) -> Self {
        assert_eq!(self.stack_id, other.stack_id);
        let mut merged_register = BTreeMap::new();
        for (register, other_value) in other.register.iter() {
            if let Some(value) = self.register.get(register) {
                let merged_value = value.merge(other_value);
                if !merged_value.is_top() {
                    // We only have to keep non-*Top* elements.
                    merged_register.insert(register.clone(), merged_value);
                }
            }
        }
        let merged_memory_objects = self.memory.merge(&other.memory);
        State {
            register: merged_register,
            memory: merged_memory_objects,
            stack_id: self.stack_id.clone(),
            caller_stack_ids: self
                .caller_stack_ids
                .union(&other.caller_stack_ids)
                .cloned()
                .collect(),
            ids_known_to_caller: self
                .ids_known_to_caller
                .union(&other.ids_known_to_caller)
                .cloned()
                .collect(),
        }
    }

    /// A state has no *Top* element
    fn is_top(&self) -> bool {
        false
    }
}

impl State {
    /// Get a more compact json-representation of the state.
    /// Intended for pretty printing, not useable for serialization/deserialization.
    pub fn to_json_compact(&self) -> serde_json::Value {
        use serde_json::*;
        let mut state_map = Map::new();
        let register = self
            .register
            .iter()
            .map(|(var, data)| (var.name.clone(), data.to_json_compact()))
            .collect();
        let register = Value::Object(register);
        state_map.insert("register".into(), register);
        state_map.insert("memory".into(), self.memory.to_json_compact());
        state_map.insert(
            "stack_id".into(),
            Value::String(format!("{}", self.stack_id)),
        );
        state_map.insert(
            "caller_stack_ids".into(),
            Value::Array(
                self.caller_stack_ids
                    .iter()
                    .map(|id| Value::String(format!("{}", id)))
                    .collect(),
            ),
        );
        state_map.insert(
            "ids_known_to_caller".into(),
            Value::Array(
                self.ids_known_to_caller
                    .iter()
                    .map(|id| Value::String(format!("{}", id)))
                    .collect(),
            ),
        );

        Value::Object(state_map)
    }
}

#[cfg(test)]
mod tests;

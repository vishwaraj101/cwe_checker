use super::{Blk, Datatype, Expression, Project, Variable};
use crate::prelude::*;

/// A `Sub` or subroutine represents a function with a given name and a list of basic blocks belonging to it.
///
/// Subroutines are *single-entry*,
/// i.e. calling a subroutine will execute the first block in the list of basic blocks.
/// A subroutine may have multiple exits, which are identified by `Jmp::Return` instructions.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Hash, Clone)]
pub struct Sub {
    /// The name of the subroutine
    pub name: String,
    /// The basic blocks belonging to the subroutine.
    /// The first block is also the entry point of the subroutine.
    pub blocks: Vec<Term<Blk>>,
    /// The calling convention used to call if known
    pub calling_convention: Option<String>,
}

/// A parameter or return argument of a function.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Hash, Clone)]
pub enum Arg {
    /// The argument is passed in a register
    Register {
        /// The expression evaluating to the argument.
        expr: Expression,
        /// An optional data type indicator.
        data_type: Option<Datatype>,
    },
    /// The argument is passed on the stack.
    Stack {
        /// The expression that computes the address of the argument on the stack.
        address: Expression,
        /// The size in bytes of the argument.
        size: ByteSize,
        /// An optional data type indicator.
        data_type: Option<Datatype>,
    },
}

impl Arg {
    /// Generate a new register argument.
    pub fn from_var(var: Variable, data_type_hint: Option<Datatype>) -> Arg {
        Arg::Register {
            expr: Expression::Var(var),
            data_type: data_type_hint,
        }
    }

    /// Returns the data type field of an Arg object.
    pub fn get_data_type(&self) -> Option<Datatype> {
        match self {
            Arg::Register { data_type, .. } => data_type.clone(),
            Arg::Stack { data_type, .. } => data_type.clone(),
        }
    }

    /// If the argument is a stack argument,
    /// return its offset relative to the current stack register value.
    /// Return an error for register arguments or if the offset could not be computed.
    pub fn eval_stack_offset(&self, stack_register: &Variable) -> Result<Bitvector, Error> {
        let expression = match self {
            Arg::Register { .. } => return Err(anyhow!("The argument is not a stack argument.")),
            Arg::Stack { address, .. } => address,
        };
        Self::eval_stack_offset_expression(expression, stack_register)
    }

    /// If the given expression computes a constant offset to the given stack register,
    /// then return the offset.
    /// Else return an error.
    fn eval_stack_offset_expression(
        expression: &Expression,
        stack_register: &Variable,
    ) -> Result<Bitvector, Error> {
        match expression {
            Expression::Var(var) => {
                if var == stack_register {
                    Ok(Bitvector::zero(var.size.into()))
                } else {
                    Err(anyhow!("Input register is not the stack register"))
                }
            }
            Expression::Const(bitvec) => Ok(bitvec.clone()),
            Expression::BinOp { op, lhs, rhs } => {
                let lhs = Self::eval_stack_offset_expression(lhs, stack_register)?;
                let rhs = Self::eval_stack_offset_expression(rhs, stack_register)?;
                lhs.bin_op(*op, &rhs)
            }
            Expression::UnOp { op, arg } => {
                let arg = Self::eval_stack_offset_expression(arg, stack_register)?;
                arg.un_op(*op)
            }
            _ => Err(anyhow!("Expression type not supported for argument values")),
        }
    }
}

/// An extern symbol represents a funtion that is dynamically linked from another binary.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Hash, Clone)]
pub struct ExternSymbol {
    /// The term ID of the extern symbol.
    pub tid: Tid,
    /// Addresses of possibly multiple locations of the same extern symbol
    pub addresses: Vec<String>,
    /// The name of the extern symbol
    pub name: String,
    /// The calling convention used for the extern symbol if known
    pub calling_convention: Option<String>,
    /// Parameters of an extern symbol.
    /// May be empty if there are no parameters or the parameters are unknown.
    pub parameters: Vec<Arg>,
    /// Return values of an extern symbol.
    /// May be empty if there is no return value or the return values are unknown.
    pub return_values: Vec<Arg>,
    /// If set to `true`, the function is assumed to never return to its caller when called.
    pub no_return: bool,
    /// If the function has a variable number of parameters, this flag is set to `true`.
    pub has_var_args: bool,
}

impl ExternSymbol {
    /// If the extern symbol has exactly one return value that is passed in a register,
    /// return the register.
    pub fn get_unique_return_register(&self) -> Result<&Variable, Error> {
        if self.return_values.len() == 1 {
            match self.return_values[0] {
                Arg::Register {
                    expr: Expression::Var(ref var),
                    ..
                } => Ok(var),
                Arg::Register { .. } => Err(anyhow!("Return value is a sub-register")),
                Arg::Stack { .. } => Err(anyhow!("Return value is passed on the stack")),
            }
        } else {
            Err(anyhow!("Wrong number of return values"))
        }
    }

    /// If the extern symbol has exactly one parameter, return the parameter.
    pub fn get_unique_parameter(&self) -> Result<&Arg, Error> {
        if self.parameters.len() == 1 {
            Ok(&self.parameters[0])
        } else {
            Err(anyhow!("Wrong number of parameter values"))
        }
    }

    /// Get the calling convention corresponding to the extern symbol.
    pub fn get_calling_convention<'a>(&self, project: &'a Project) -> &'a CallingConvention {
        project.get_calling_convention(self)
    }
}

/// Calling convention related data
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Hash, Clone)]
pub struct CallingConvention {
    /// The name of the calling convention
    #[serde(rename = "calling_convention")]
    pub name: String,
    /// Possible integer parameter registers.
    pub integer_parameter_register: Vec<Variable>,
    /// Possible float parameter registers.
    /// Given as expressions, since they are usually sub-register of larger floating point registers.
    pub float_parameter_register: Vec<Expression>,
    /// A list of possible return register for non-float values.
    pub integer_return_register: Vec<Variable>,
    /// A list of possible return register for float values.
    /// Given as expressions, since they are usually sub-register of larger floating point registers.
    pub float_return_register: Vec<Expression>,
    /// A list of callee-saved register,
    /// i.e. the values of these registers should be the same after the call as they were before the call.
    pub callee_saved_register: Vec<Variable>,
}

impl CallingConvention {
    /// Return a list of all parameter registers of the calling convention.
    /// For parameters, where only a part of a register is the actual parameter,
    /// the parameter register is approximated by the (larger) base register.
    pub fn get_all_parameter_register(&self) -> Vec<&Variable> {
        let mut register_list: Vec<&Variable> = self.integer_parameter_register.iter().collect();
        for float_param_expr in self.float_parameter_register.iter() {
            register_list.append(&mut float_param_expr.input_vars());
        }
        register_list
    }

    /// Return a list of all return registers of the calling convention.
    /// For return register, where only a part of a register is the actual return register,
    /// the return register is approximated by the (larger) base register.
    pub fn get_all_return_register(&self) -> Vec<&Variable> {
        let mut register_list: Vec<&Variable> = self.integer_return_register.iter().collect();
        for float_param_expr in self.float_return_register.iter() {
            register_list.append(&mut float_param_expr.input_vars());
        }
        register_list
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl Sub {
        pub fn mock(name: impl ToString) -> Term<Sub> {
            Term {
                tid: Tid::new(name.to_string()),
                term: Sub {
                    name: name.to_string(),
                    blocks: Vec::new(),
                    calling_convention: None,
                },
            }
        }
    }

    impl CallingConvention {
        pub fn mock() -> CallingConvention {
            CallingConvention {
                name: "__stdcall".to_string(), // so that the mock is useable as standard calling convention in tests
                integer_parameter_register: vec![Variable::mock("RDI", 8)],
                float_parameter_register: vec![Expression::Var(Variable::mock("XMMO", 16))],
                integer_return_register: vec![Variable::mock("RAX", 8)],
                float_return_register: vec![],
                callee_saved_register: vec![Variable::mock("RBP", 8)],
            }
        }

        pub fn mock_arm32() -> CallingConvention {
            CallingConvention {
                name: "__stdcall".to_string(), // so that the mock is useable as standard calling convention in tests
                integer_parameter_register: vec![Variable::mock("r0", 4)],
                float_parameter_register: vec![Expression::Var(Variable::mock("d0", 8))],
                integer_return_register: vec![Variable::mock("r0", 4)],
                float_return_register: vec![],
                callee_saved_register: vec![Variable::mock("r4", 4)],
            }
        }

        pub fn mock_with_parameter_registers(
            integer_parameter_register: Vec<Variable>,
            float_parameter_register: Vec<Variable>,
        ) -> CallingConvention {
            let float_parameter_register = float_parameter_register
                .into_iter()
                .map(Expression::Var)
                .collect();
            CallingConvention {
                name: "__stdcall".to_string(), // so that the mock is useable as standard calling convention in tests
                integer_parameter_register,
                float_parameter_register,
                integer_return_register: vec![Variable::mock("RAX", 8)],
                float_return_register: vec![],
                callee_saved_register: vec![Variable::mock("RBP", 8)],
            }
        }
    }

    impl Arg {
        pub fn mock_register(name: impl ToString, size_in_bytes: impl Into<ByteSize>) -> Arg {
            Arg::Register {
                expr: Expression::Var(Variable::mock(name.to_string(), size_in_bytes)),
                data_type: None,
            }
        }

        pub fn mock_register_with_data_type(
            name: impl ToString,
            size_in_bytes: impl Into<ByteSize>,
            data_type: Option<Datatype>,
        ) -> Arg {
            Arg::Register {
                expr: Expression::Var(Variable::mock(name.to_string(), size_in_bytes)),
                data_type,
            }
        }

        pub fn mock_pointer_register(
            name: impl ToString,
            size_in_bytes: impl Into<ByteSize>,
        ) -> Arg {
            Arg::Register {
                expr: Expression::Var(Variable::mock(name.to_string(), size_in_bytes)),
                data_type: Some(Datatype::Pointer),
            }
        }
    }

    impl ExternSymbol {
        pub fn mock_x64() -> ExternSymbol {
            ExternSymbol {
                tid: Tid::new("mock_symbol"),
                addresses: vec!["UNKNOWN".to_string()],
                name: "mock_symbol".to_string(),
                calling_convention: Some("__stdcall".to_string()),
                parameters: vec![Arg::mock_register("RDI", 8)],
                return_values: vec![Arg::mock_register("RAX", 8)],
                no_return: false,
                has_var_args: false,
            }
        }

        pub fn mock_arm32() -> ExternSymbol {
            // There is also the mock_standard_arm32() method. Only on of the two should exist!
            ExternSymbol {
                tid: Tid::new("mock_symbol"),
                addresses: vec!["UNKNOWN".to_string()],
                name: "mock_symbol".to_string(),
                calling_convention: Some("__stdcall".to_string()),
                parameters: vec![Arg::mock_register("r0", 4)],
                return_values: vec![Arg::mock_register("r0", 4)],
                no_return: false,
                has_var_args: false,
            }
        }

        pub fn mock_string() -> Self {
            ExternSymbol {
                tid: Tid::new("sprintf"),
                addresses: vec!["UNKNOWN".to_string()],
                name: "sprintf".to_string(),
                calling_convention: Some("__stdcall".to_string()),
                parameters: vec![Arg::mock_register("RDI", 8), Arg::mock_register("RSI", 8)],
                return_values: vec![Arg::mock_register("RAX", 8)],
                no_return: false,
                has_var_args: true,
            }
        }
    }
}

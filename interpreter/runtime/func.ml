open Types
open Values

type 'inst t = 'inst func
and 'inst func =
  | AstFunc of func_type * 'inst * Ast.func
  | HostFunc of func_type * (value list -> value list)
  | GasIntrinsic
  | StackIntrinsic
  | UnstackIntrinsic

let alloc ft inst f = AstFunc (ft, inst, f)
let alloc_host ft f = HostFunc (ft, f)

let type_of = function
  | AstFunc (ft, _, _) -> ft
  | HostFunc (ft, _) -> ft
  | GasIntrinsic -> (FuncType ([NumType I64Type], []))
  | StackIntrinsic -> (FuncType ([NumType I64Type; NumType I64Type], []))
  | UnstackIntrinsic -> (FuncType ([NumType I64Type; NumType I64Type], []))

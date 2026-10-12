// `inert` keeps keyboard focus, clicks and screen readers out of what is
// mounted but not shown: a closed slide-over, a session behind another view.
// React 18 doesn't know the attribute and passes `inert=""` through, which is
// all it needs. React 19 types it as a boolean and reads "" as false; this
// declaration then clashes with React's own, so an upgrade stops at compile
// time until the callers pass `inert={true}` instead.
import "react";

declare module "react" {
  interface HTMLAttributes<T> {
    inert?: "";
  }
}

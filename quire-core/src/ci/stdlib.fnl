;; quire.stdlib — helpers callable from inside any run-fn via
;; `(require :quire.stdlib)`. Each function pulls its runtime
;; primitives from `(. (require :quire.ci) :runtime)` at call time so
;; the binding always tracks the currently-installed runtime.

(local M {})

M

;; quire.ci macros — imported via `(import-macros {: defrun} :quire.ci)`.
;;
;; `defrun` is sugar for the common run-fn shape: a zero-arg function
;; whose body needs some keys (`sh`, `secret`, `jobs`, `mirror`, …) from
;; the runtime. The arglist names the keys to pull in, and the macro
;; emits the `let` that binds them.
;;
;;   (defrun [sh jobs]
;;     (let [push (jobs :quire/push)]
;;       (sh ["cargo" "test"])))
;;
;; expands to:
;;
;;   (fn []
;;     (let [{: sh : jobs} (. (require :quire.ci) :runtime)]
;;       (let [push (jobs :quire/push)]
;;         (sh ["cargo" "test"]))))
;;
;; An empty arglist skips the `let` entirely:
;;
;;   (defrun [] (do-something))  =>  (fn [] (do-something))
;;
;; For renaming, nested destructures, or anything else beyond a flat
;; key-grab, write the `let` by hand instead of using `defrun`.

(fn defrun [arglist ...]
  (let [body [...]
        destructure {}]
    (each [_ name (ipairs arglist)]
      (assert-compile (sym? name)
                      "defrun arglist must be a sequence of bare symbols naming runtime keys"
                      name)
      (tset destructure (tostring name) name))
    (if (= 0 (length arglist))
        `(fn []
           ,(unpack body))
        `(fn []
           (let [,destructure (. (require :quire.ci) :runtime)]
             ,(unpack body))))))

{: defrun}

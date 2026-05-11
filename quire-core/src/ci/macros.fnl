;; quire.ci macros — imported via `(import-macros {: defrun} :quire.ci)`.
;;
;; `defrun` is sugar for the common run-fn shape: a zero-arg function
;; whose body needs `sh` / `secret` / `jobs` / `mirror` from the
;; ambient `runtime` global. Writing `(let [{: sh} runtime] …)` at the
;; top of every job becomes the macro itself, with the destructure
;; pattern as the apparent argument list.
;;
;;   (defrun [{: sh : jobs}]
;;     (let [push (jobs :quire/push)]
;;       (sh ["cargo" "test"])))
;;
;; expands to:
;;
;;   (fn []
;;     (let [{: sh : jobs} runtime]
;;       (let [push (jobs :quire/push)]
;;         (sh ["cargo" "test"]))))
;;
;; An empty arglist skips the `let` entirely:
;;
;;   (defrun [] (do-something))  =>  (fn [] (do-something))

(fn defrun [arglist ...]
  (assert-compile (<= (length arglist) 1)
                  "defrun expects an arglist with 0 or 1 destructure pattern"
                  arglist)
  (let [body [...]]
    (if (= 0 (length arglist))
        `(fn []
           ,(unpack body))
        `(fn []
           (let [,(. arglist 1) runtime]
             ,(unpack body))))))

{: defrun}

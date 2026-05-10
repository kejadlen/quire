;; quire.stdlib — helpers callable from inside any run-fn via
;; `(require :quire.stdlib)`. Each function pulls its runtime
;; primitives from `(require :quire.runtime)` at call time so the
;; binding always tracks the currently-installed runtime.

(local M {})

(fn trim [s]
  (string.gsub s "%s+$" ""))

(fn cat [...]
  ;; Concatenate sequence tables into a fresh sequence.
  (let [out []]
    (each [_ t (ipairs [...])]
      (each [_ x (ipairs t)]
        (table.insert out x)))
    out))

;; (mirror opts)
;;
;; Tag a commit and push the tag (plus optional refs) to a remote.
;;
;; opts: {:url         — remote URL (required)
;;        :auth-header — full HTTP header line passed to git as
;;                       `http.extraHeader`; resolve via
;;                       `runtime.secret` at the call site (required)
;;        :sha         — commit to tag (required)
;;        :tag         — tag name (required)
;;        :git-dir     — bare git directory the run is scoped to (required)
;;        :refs        — extra refs to push alongside the tag
;;                       (optional, default [])}
;;
;; Returns {:tag :pushed_refs}. Raises on missing required opts or
;; non-zero git exits. `lambda` checks the required bindings for nil
;; at the call site.
(λ M.mirror [{: url : auth-header : sha : tag : git-dir :refs ?refs}]
  (let [{: sh} (require :quire.runtime)
        refs (or ?refs [])
        ;; Pass http.extraHeader via GIT_CONFIG_* env (git 2.31+)
        ;; instead of `-c http.extraHeader=…` in argv. Keeps the auth
        ;; header out of `ps` and out of any argv logging we add
        ;; later; runtime.sh's redact pass on stdout/stderr remains as
        ;; defense in depth.
        sh-opts {:env {:GIT_DIR git-dir
                       :GIT_CONFIG_COUNT :1
                       :GIT_CONFIG_KEY_0 :http.extraHeader
                       :GIT_CONFIG_VALUE_0 auth-header}}
        tag-result (sh [:git :tag tag sha] sh-opts)]
    (when (not= 0 tag-result.exit)
      (error (.. "git tag failed: " (trim tag-result.stderr))))
    (let [push-args (cat [:git :push :--porcelain url]
                         refs
                         [(.. :refs/tags/ tag)])
          push-result (sh push-args sh-opts)]
      (when (not= 0 push-result.exit)
        (error (.. "git push failed: " (trim push-result.stderr))))
      {: tag :pushed_refs refs})))

M

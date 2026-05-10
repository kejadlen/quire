;; quire.stdlib — helpers callable from inside any run-fn via
;; `(require :quire.stdlib)`. Each function pulls its runtime
;; primitives from `(require :quire.runtime)` at call time so the
;; binding always tracks the currently-installed runtime.

(local M {})

(fn missing [field]
  (error (.. "quire.stdlib.mirror: missing required option :" field)))

(fn trim [s]
  (string.gsub s "%s+$" ""))

;; (mirror opts)
;;
;; Tag a commit and push the tag (plus optional refs) to a remote.
;;
;; opts: {:url       — remote URL (required)
;;        :secret    — secret name resolved via runtime.secret (required)
;;        :sha       — commit to tag (required)
;;        :tag       — tag name (required)
;;        :git-dir   — bare git directory the run is scoped to (required)
;;        :refs      — extra refs to push alongside the tag (optional, default [])}
;;
;; Returns {:tag :pushed_refs}. Raises on missing required opts,
;; unknown secrets, or non-zero git exits.
(fn M.mirror [opts]
  (let [{: secret : sh} (require :quire.runtime)
        url (or opts.url (missing :url))
        secret-name (or opts.secret (missing :secret))
        sha (or opts.sha (missing :sha))
        tag (or opts.tag (missing :tag))
        git-dir (or (. opts :git-dir) (missing :git-dir))
        refs (or opts.refs [])
        auth-header (secret secret-name)
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
    (let [push-args [:git :push :--porcelain url]]
      (each [_ ref (ipairs refs)]
        (table.insert push-args ref))
      (table.insert push-args (.. :refs/tags/ tag))
      (let [push-result (sh push-args sh-opts)]
        (when (not= 0 push-result.exit)
          (error (.. "git push failed: " (trim push-result.stderr))))
        {: tag :pushed_refs refs}))))

M

(local {: job : mirror} (require :quire.ci))
(import-macros {: defrun} :quire.ci)

; (job :test [:quire/push] (defrun [{: sh}] (sh [:cargo :test])))

(mirror "https://github.com/kejadlen/quire.git"
        {:refs [:refs/heads/main]
         :secret :github_auth_header
         :tag (fn [{: sha}]
                (.. :v (os.date "!%Y-%m-%d") "-" (sha:sub 1 8)))})

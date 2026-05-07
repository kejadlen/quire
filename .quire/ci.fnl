(local {: job : mirror} (require :quire.ci))

(mirror "https://github.com/kejadlen/quire.git"
        {:refs [:refs/heads/main]
         :secret :github_auth_header
         :tag (fn [{: sha}]
                (.. :v (os.date "!%Y-%m-%d") "-" (sha:sub 1 8)))})

; (job :test [:quire/push] (fn [{: sh}] (sh [:cargo :test])))

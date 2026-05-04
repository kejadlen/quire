(local {: mirror} (require :quire.ci))

(mirror "https://github.com/kejadlen/quire.git"
        {:refs [:refs/heads/main]
         :secret :github_token
         :tag (fn [{: sha}]
                (.. :v (os.date "!%Y-%m-%d") "-" (sha:sub 1 8)))})


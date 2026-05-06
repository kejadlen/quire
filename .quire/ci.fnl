(local {: job : mirror} (require :quire.ci))

(local mirror-url "https://github.com/kejadlen/quire.git")

(job :tag-and-mirror [:quire/push]
     (fn [{: sh : secret : jobs}]
       (let [{: ref : sha : git-dir} (jobs :quire/push)
             token (secret :github_token)]
         (when (= ref :refs/heads/main)
           (let [date (-> (sh "date --utc +%Y-%m-%d")
                          (. :stdout)
                          (: :gsub "\n$" ""))
                 tag (.. :v date "-" (sha:sub 1 8))
                 encoded (-> (sh "printf '%s' \"$T\" | base64 --wrap=0"
                                 {:env {:T (.. "x-access-token:" token)}})
                             (. :stdout))
                 auth-header (.. "Authorization: Basic " encoded)
                 git-opts {:env {:GIT_DIR git-dir}}]
             (sh [:git :tag tag sha] git-opts)
             (sh [:git
                  :-c
                  (.. :http.extraHeader= auth-header)
                  :push
                  :--porcelain
                  mirror-url
                  :refs/heads/main
                  (.. :refs/tags/ tag)] git-opts))))))

; (mirror "https://github.com/kejadlen/quire.git"
;         {:refs [:refs/heads/main]
;          :secret :github_token
;          :tag (fn [{: sha}]
;                 (.. :v (os.date "!%Y-%m-%d") "-" (sha:sub 1 8)))})

; (job :test [:quire/push] (fn [{: sh}] (sh [:cargo :test])))

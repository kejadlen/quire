(local {: job} (require :quire.ci))
(local mirror-url "https://github.com/kejadlen/quire.git")

(job :tag-and-mirror [:quire/push]
     (fn [{: sh : secret : jobs}]
       (let [{: ref : sha} (jobs :quire/push)
             token (secret :github_token)]
         (when (= ref "refs/heads/main")
           (let [date (-> (sh "date --utc +%Y-%m-%d")
                          (. :stdout)
                          (: :gsub "\n$" ""))
                 tag (.. :v date "+" (sha:sub 1 12))
                 encoded (-> (sh "printf '%s' \"$T\" | base64"
                                 {:env {:T (.. "x-access-token:" token)}})
                             (. :stdout)
                             (: :gsub "\n$" ""))
                 auth-header (.. "Authorization: Basic " encoded)]
             (sh [:git :tag tag sha])
             (sh [:git
                  :-c
                  (.. :http.extraHeader= auth-header)
                  :push
                  :--porcelain
                  mirror-url
                  (.. :refs/tags/ tag)]))))))

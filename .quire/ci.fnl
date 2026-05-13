(local {: job} (require :quire.ci))
(local {: mirror} (require :quire.stdlib))

; (job :test [:quire/push] (fn [] (runtime.sh [:cargo :test])))

(job "quire/mirror" [:quire/push]
  (fn []
    (let [{: jobs : secret} (. (require :quire.ci) :runtime)
          push (jobs :quire/push)]
      (when (= push.ref "refs/heads/main")
        (mirror {:url "https://github.com/kejadlen/quire.git"
                 :auth-header (secret :github_auth_header)
                 :sha push.sha
                 :tag (.. "v" (os.date "!%Y-%m-%d") "-" (push.sha:sub 1 8))
                 :git-dir push.git-dir
                 :refs ["refs/heads/main"]})))))

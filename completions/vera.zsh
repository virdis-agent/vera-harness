#compdef vera

_arguments \
  '1:repository:_directories' \
  '(-p --prompt)'{-p,--prompt}'[headless prompt]:prompt:' \
  '--output=[output format]:format:(text jsonl)' \
  '--provider=[provider id]:provider:(openai-codex xai-oauth)' \
  '--model=[model id]:model:' \
  '--effort=[reasoning effort]:effort:(auto low medium high)' \
  '--prompt-template=[reusable prompt name]:name:' \
  '--prompt-args=[reusable prompt arguments]:args:' \
  '1:command:(auth models session inspect mcp plugin update)' \
  '*::argument:->args'

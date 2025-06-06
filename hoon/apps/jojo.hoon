/+  jock
/=  *  /common/wrapper
::
=>
|%
+$  versioned-state
  $:  %v1
      ~
  ==
::
+$  effect
  $%  [%log str=@t]
  ==
::
+$  cause
  $%  [%command str=@t]
  ==
--
|%
++  moat  (keep versioned-state)
::
++  inner
  |_  state=versioned-state
  ::
  ++  load
    |=  old-state=versioned-state
    ^-  _state
    ?:  =(-.old-state %v1)
      old-state
    old-state
  ::
  ++  peek
    |=  =path
    ^-  (unit (unit *))
    ~
  ::
  ++  poke
    |=  =ovum:moat
    ^-  [(list effect) _state]
    |^
    =/  cause  ((soft cause) cause.input.ovum)
    ?~  cause
      ~>  %slog.[3 (crip "invalid cause {<cause.input.ovum>}")]
      :_  state
      [%log 'Invalid cause format']~
    ~>  %slog.[1 (cat 3 'poked: ' str.u.cause)]
    :_  state
    =/  min=(each nock tang)
      ;;  (each nock tang)
      (mule |.((mint:jock str.u.cause)))
    ?:  ?=(%| -.min)
      ~&  >>  'failed to mint jock code'
      [%log (tang-to-cord p.min)]~
    =/  res=(each noun tang)
      ;;  (each noun tang)
      (mule |.(.*(%0 p.min)))
    ?:  ?=(%| -.res)
      ~&  >>  'failed to execute nock code'
      [%log (tang-to-cord p.res)]~
    [%log (crip "{<p.res>}")]~
    ::
    ++  tang-to-cord
      |=  =tang
      ^-  cord
      %+  rap  3
      %+  join  '\0a'
      ^-  wain
      %+  turn  tang
      |=  t=tank
      (rap 3 (join '\0a' (turn (wash [0 80] t) crip)))
    --
  --
--
((moat |) inner)

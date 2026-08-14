# Composable templates + a service that merges them, per the design
# doc's "Composition" section.

template internal_web(port) {
  networks [traefik-net]
  restart unless-stopped
  expose port as "{{name}}.internal.techdebtor.io"
  middleware local-ipwhitelist
}

template authenticated {
  middleware forwardAuth-authentik
}

template linuxserver_app(puid, pgid) {
  env PUID = puid
  env PGID = pgid
}

service syncthing {
  with internal_web { port: 8384 }, authenticated, linuxserver_app { puid: 1000, pgid: 100 }
  image "lscr.io/linuxserver/syncthing:latest"
  volume "syncthing-config" -> "/config"
}

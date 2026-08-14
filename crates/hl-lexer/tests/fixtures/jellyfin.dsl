service jellyfin {
  image "jellyfin/jellyfin:latest"
  expose 8096 as "media.techdebtor.io"
  volume "/mnt/media" -> "/data"
  env PUID = "1000"
  restart unless-stopped
}

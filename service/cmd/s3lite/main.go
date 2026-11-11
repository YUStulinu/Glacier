// Command s3lite runs a minimal S3-compatible server for local testing.
//
//	S3LITE_ACCESS_KEY=... S3LITE_SECRET_KEY=... s3lite -addr 127.0.0.1:9300 -dir ./s3data
package main

import (
	"flag"
	"log"
	"net/http"
	"os"

	"github.com/YUStulinu/Glacier/service/s3lite"
)

func main() {
	addr := flag.String("addr", "127.0.0.1:9300", "listen address")
	dir := flag.String("dir", "s3data", "where objects are stored")
	flag.Parse()
	access, secret := os.Getenv("S3LITE_ACCESS_KEY"), os.Getenv("S3LITE_SECRET_KEY")
	if access == "" || secret == "" {
		log.Fatal("set S3LITE_ACCESS_KEY and S3LITE_SECRET_KEY")
	}
	if err := os.MkdirAll(*dir, 0o755); err != nil {
		log.Fatal(err)
	}
	log.Printf("s3lite on http://%s, objects in %s", *addr, *dir)
	log.Fatal(http.ListenAndServe(*addr, &s3lite.Server{Dir: *dir, Access: access, Secret: secret, Region: "us-east-1"}))
}

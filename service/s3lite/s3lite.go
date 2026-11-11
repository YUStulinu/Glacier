// Package s3lite is a minimal S3-compatible object store for testing Glacier's S3 backend
// locally: PUT, GET (with Range), HEAD and DELETE of objects, and ListObjectsV2, with
// Signature Version 4 authentication. Objects are files under a directory, one subdirectory
// per bucket; buckets are created on first write.
package s3lite

import (
	"crypto/hmac"
	"crypto/sha256"
	"crypto/subtle"
	"encoding/hex"
	"encoding/xml"
	"fmt"
	"io"
	"io/fs"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"time"
)

type Server struct {
	Dir    string
	Access string
	Secret string
	Region string
	// Now is replaceable in tests; requests more than 15 minutes off are refused.
	Now func() time.Time
}

func hmacSHA256(key []byte, msg string) []byte {
	m := hmac.New(sha256.New, key)
	m.Write([]byte(msg))
	return m.Sum(nil)
}

func sha256Hex(b []byte) string {
	h := sha256.Sum256(b)
	return hex.EncodeToString(h[:])
}

// URIEncode encodes as SigV4 requires (RFC 3986 unreserved characters kept; '/' kept in paths).
func URIEncode(s string, keepSlash bool) string {
	var b strings.Builder
	for i := 0; i < len(s); i++ {
		c := s[i]
		switch {
		case c >= 'A' && c <= 'Z', c >= 'a' && c <= 'z', c >= '0' && c <= '9', c == '-', c == '_', c == '.', c == '~':
			b.WriteByte(c)
		case c == '/' && keepSlash:
			b.WriteByte(c)
		default:
			fmt.Fprintf(&b, "%%%02X", c)
		}
	}
	return b.String()
}

// Signature computes a SigV4 signature. headers maps lower-case names to values; every one is
// signed.
func Signature(method, path string, query url.Values, headers map[string]string, payloadHash, amzDate, region, secret string) (signature, signedHeaders string) {
	var qs []string
	for k, vs := range query {
		for _, v := range vs {
			qs = append(qs, URIEncode(k, false)+"="+URIEncode(v, false))
		}
	}
	sort.Strings(qs)
	names := make([]string, 0, len(headers))
	for k := range headers {
		names = append(names, k)
	}
	sort.Strings(names)
	var ch strings.Builder
	for _, k := range names {
		fmt.Fprintf(&ch, "%s:%s\n", k, strings.TrimSpace(headers[k]))
	}
	signedHeaders = strings.Join(names, ";")
	creq := strings.Join([]string{method, URIEncode(path, true), strings.Join(qs, "&"), ch.String(), signedHeaders, payloadHash}, "\n")
	date := amzDate[:8]
	scope := date + "/" + region + "/s3/aws4_request"
	sts := "AWS4-HMAC-SHA256\n" + amzDate + "\n" + scope + "\n" + sha256Hex([]byte(creq))
	k := hmacSHA256(hmacSHA256(hmacSHA256(hmacSHA256([]byte("AWS4"+secret), date), region), "s3"), "aws4_request")
	return hex.EncodeToString(hmacSHA256(k, sts)), signedHeaders
}

// Sign adds SigV4 headers to a request (used by tests and by the Go service's health check).
func Sign(r *http.Request, body []byte, access, secret, region string, now time.Time) {
	amz := now.UTC().Format("20060102T150405Z")
	ph := sha256Hex(body)
	r.Header.Set("x-amz-date", amz)
	r.Header.Set("x-amz-content-sha256", ph)
	h := map[string]string{"host": r.URL.Host, "x-amz-date": amz, "x-amz-content-sha256": ph}
	if rg := r.Header.Get("Range"); rg != "" {
		h["range"] = rg
	}
	sig, signed := Signature(r.Method, r.URL.Path, r.URL.Query(), h, ph, amz, region, secret)
	r.Header.Set("Authorization", fmt.Sprintf("AWS4-HMAC-SHA256 Credential=%s/%s/%s/s3/aws4_request, SignedHeaders=%s, Signature=%s", access, amz[:8], region, signed, sig))
}

func (s *Server) authorize(r *http.Request, body []byte) error {
	auth := r.Header.Get("Authorization")
	if !strings.HasPrefix(auth, "AWS4-HMAC-SHA256 ") {
		return fmt.Errorf("missing SigV4 authorization")
	}
	fields := map[string]string{}
	for _, p := range strings.Split(strings.TrimPrefix(auth, "AWS4-HMAC-SHA256 "), ",") {
		k, v, _ := strings.Cut(strings.TrimSpace(p), "=")
		fields[k] = v
	}
	cred := strings.Split(fields["Credential"], "/")
	if len(cred) != 5 || cred[0] != s.Access {
		return fmt.Errorf("unknown access key")
	}
	amz := r.Header.Get("x-amz-date")
	t, err := time.Parse("20060102T150405Z", amz)
	if err != nil {
		return fmt.Errorf("bad x-amz-date")
	}
	now := time.Now
	if s.Now != nil {
		now = s.Now
	}
	if d := now().Sub(t); d > 15*time.Minute || d < -15*time.Minute {
		return fmt.Errorf("request time too skewed")
	}
	ph := r.Header.Get("x-amz-content-sha256")
	if ph != "UNSIGNED-PAYLOAD" && ph != sha256Hex(body) {
		return fmt.Errorf("payload hash mismatch")
	}
	h := map[string]string{}
	for _, name := range strings.Split(fields["SignedHeaders"], ";") {
		if name == "host" {
			h[name] = r.Host
		} else {
			h[name] = r.Header.Get(name)
		}
	}
	region := cred[2]
	sig, _ := Signature(r.Method, r.URL.Path, r.URL.Query(), h, ph, amz, region, s.Secret)
	if subtle.ConstantTimeCompare([]byte(sig), []byte(fields["Signature"])) != 1 {
		return fmt.Errorf("signature does not match")
	}
	return nil
}

func xmlError(w http.ResponseWriter, code int, kind, msg string) {
	w.Header().Set("Content-Type", "application/xml")
	w.WriteHeader(code)
	fmt.Fprintf(w, "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>%s</Code><Message>%s</Message></Error>", kind, xmlEscape(msg))
}

func xmlEscape(s string) string {
	var b strings.Builder
	xml.EscapeText(&b, []byte(s))
	return b.String()
}

func validName(s string) bool {
	if s == "" || strings.Contains(s, "\\") || strings.HasPrefix(s, "/") {
		return false
	}
	for _, p := range strings.Split(s, "/") {
		if p == "" || p == "." || p == ".." {
			return false
		}
	}
	return true
}

func (s *Server) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	body, err := io.ReadAll(io.LimitReader(r.Body, 1<<30))
	if err != nil {
		xmlError(w, 400, "IncompleteBody", err.Error())
		return
	}
	if err := s.authorize(r, body); err != nil {
		xmlError(w, 403, "SignatureDoesNotMatch", err.Error())
		return
	}
	bucket, key, _ := strings.Cut(strings.TrimPrefix(r.URL.Path, "/"), "/")
	if !validName(bucket) || strings.Contains(bucket, "/") {
		xmlError(w, 400, "InvalidBucketName", bucket)
		return
	}
	if key == "" {
		if r.Method == http.MethodGet {
			s.list(w, r, bucket)
			return
		}
		if r.Method == http.MethodPut || r.Method == http.MethodHead {
			os.MkdirAll(filepath.Join(s.Dir, bucket), 0o755)
			w.WriteHeader(200)
			return
		}
		xmlError(w, 405, "MethodNotAllowed", r.Method)
		return
	}
	if !validName(key) {
		xmlError(w, 400, "InvalidKey", key)
		return
	}
	path := filepath.Join(s.Dir, bucket, filepath.FromSlash(key))
	switch r.Method {
	case http.MethodPut:
		if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
			xmlError(w, 500, "InternalError", err.Error())
			return
		}
		tmp := path + ".s3lite-tmp"
		f, err := os.Create(tmp)
		if err == nil {
			_, err = f.Write(body)
			if err == nil {
				err = f.Sync()
			}
			f.Close()
		}
		if err == nil {
			err = os.Rename(tmp, path)
		}
		if err != nil {
			os.Remove(tmp)
			xmlError(w, 500, "InternalError", err.Error())
			return
		}
		w.Header().Set("ETag", `"`+sha256Hex(body)[:32]+`"`)
		w.WriteHeader(200)
	case http.MethodGet, http.MethodHead:
		data, err := os.ReadFile(path)
		if err != nil {
			xmlError(w, 404, "NoSuchKey", key)
			return
		}
		w.Header().Set("Content-Type", "application/octet-stream")
		if rg := r.Header.Get("Range"); rg != "" && r.Method == http.MethodGet {
			a, b, ok := parseRange(rg, int64(len(data)))
			if !ok {
				xmlError(w, 416, "InvalidRange", rg)
				return
			}
			w.Header().Set("Content-Range", fmt.Sprintf("bytes %d-%d/%d", a, b, len(data)))
			w.Header().Set("Content-Length", strconv.FormatInt(b-a+1, 10))
			w.WriteHeader(206)
			w.Write(data[a : b+1])
			return
		}
		w.Header().Set("Content-Length", strconv.Itoa(len(data)))
		w.WriteHeader(200)
		if r.Method == http.MethodGet {
			w.Write(data)
		}
	case http.MethodDelete:
		os.Remove(path)
		w.WriteHeader(204)
	default:
		xmlError(w, 405, "MethodNotAllowed", r.Method)
	}
}

func parseRange(h string, size int64) (int64, int64, bool) {
	spec, ok := strings.CutPrefix(h, "bytes=")
	if !ok {
		return 0, 0, false
	}
	as, bs, _ := strings.Cut(spec, "-")
	a, err1 := strconv.ParseInt(as, 10, 64)
	b, err2 := strconv.ParseInt(bs, 10, 64)
	if bs == "" {
		b, err2 = size-1, nil
	}
	if err1 != nil || err2 != nil || a < 0 || a > b || a >= size {
		return 0, 0, false
	}
	if b >= size {
		b = size - 1
	}
	return a, b, true
}

func (s *Server) list(w http.ResponseWriter, r *http.Request, bucket string) {
	q := r.URL.Query()
	prefix := q.Get("prefix")
	after := q.Get("continuation-token")
	if after == "" {
		after = q.Get("start-after")
	}
	max := 1000
	if m, err := strconv.Atoi(q.Get("max-keys")); err == nil && m > 0 && m < max {
		max = m
	}
	root := filepath.Join(s.Dir, bucket)
	var keys []string
	sizes := map[string]int64{}
	filepath.WalkDir(root, func(p string, d fs.DirEntry, err error) error {
		if err != nil || d.IsDir() || strings.HasSuffix(p, ".s3lite-tmp") {
			return nil
		}
		rel, _ := filepath.Rel(root, p)
		k := filepath.ToSlash(rel)
		if strings.HasPrefix(k, prefix) && k > after {
			keys = append(keys, k)
			if info, err := d.Info(); err == nil {
				sizes[k] = info.Size()
			}
		}
		return nil
	})
	sort.Strings(keys)
	truncated := len(keys) > max
	if truncated {
		keys = keys[:max]
	}
	var b strings.Builder
	b.WriteString(`<?xml version="1.0" encoding="UTF-8"?><ListBucketResult>`)
	fmt.Fprintf(&b, "<Name>%s</Name><Prefix>%s</Prefix><KeyCount>%d</KeyCount><MaxKeys>%d</MaxKeys><IsTruncated>%t</IsTruncated>", xmlEscape(bucket), xmlEscape(prefix), len(keys), max, truncated)
	for _, k := range keys {
		fmt.Fprintf(&b, "<Contents><Key>%s</Key><Size>%d</Size></Contents>", xmlEscape(k), sizes[k])
	}
	if truncated {
		fmt.Fprintf(&b, "<NextContinuationToken>%s</NextContinuationToken>", xmlEscape(keys[len(keys)-1]))
	}
	b.WriteString("</ListBucketResult>")
	w.Header().Set("Content-Type", "application/xml")
	io.WriteString(w, b.String())
}

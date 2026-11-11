package s3lite

import (
	"bytes"
	"io"
	"net/http"
	"net/http/httptest"
	"net/url"
	"strings"
	"testing"
	"time"
)

// The GET-object example from AWS's Signature Version 4 documentation.
func TestSignatureMatchesAWSExample(t *testing.T) {
	h := map[string]string{
		"host":                 "examplebucket.s3.amazonaws.com",
		"range":                "bytes=0-9",
		"x-amz-content-sha256": sha256Hex(nil),
		"x-amz-date":           "20130524T000000Z",
	}
	sig, signed := Signature("GET", "/test.txt", url.Values{}, h, sha256Hex(nil), "20130524T000000Z", "us-east-1", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY")
	if sig != "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41" || signed != "host;range;x-amz-content-sha256;x-amz-date" {
		t.Fatalf("%s %s", sig, signed)
	}
}

func newServer(t *testing.T) (*Server, *httptest.Server) {
	s := &Server{Dir: t.TempDir(), Access: "AK", Secret: "SK", Region: "us-east-1"}
	ts := httptest.NewServer(s)
	t.Cleanup(ts.Close)
	return s, ts
}

func do(t *testing.T, ts *httptest.Server, method, path string, body []byte, hdr map[string]string, secret string) (int, []byte) {
	req, _ := http.NewRequest(method, ts.URL+path, bytes.NewReader(body))
	for k, v := range hdr {
		req.Header.Set(k, v)
	}
	Sign(req, body, "AK", secret, "us-east-1", time.Now())
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	b, _ := io.ReadAll(resp.Body)
	return resp.StatusCode, b
}

func TestObjectsAndListing(t *testing.T) {
	_, ts := newServer(t)
	for _, k := range []string{"data/aa/1", "data/ab/2", "data/ab/3", "index/x", "config"} {
		if c, b := do(t, ts, "PUT", "/bk/"+k, []byte("v-"+k), nil, "SK"); c != 200 {
			t.Fatal(c, string(b))
		}
	}
	if c, b := do(t, ts, "GET", "/bk/data/ab/2", nil, nil, "SK"); c != 200 || string(b) != "v-data/ab/2" {
		t.Fatal(c, string(b))
	}
	if c, b := do(t, ts, "GET", "/bk/data/ab/2", nil, map[string]string{"Range": "bytes=2-5"}, "SK"); c != 206 || string(b) != "data" {
		t.Fatal(c, string(b))
	}
	// Pagination: two keys per page.
	var keys []string
	token := ""
	for page := 0; page < 5; page++ {
		q := url.Values{"list-type": {"2"}, "prefix": {"data/"}, "max-keys": {"2"}}
		if token != "" {
			q.Set("continuation-token", token)
		}
		_, b := do(t, ts, "GET", "/bk?"+q.Encode(), nil, nil, "SK")
		s := string(b)
		for _, part := range strings.Split(s, "<Key>")[1:] {
			keys = append(keys, part[:strings.Index(part, "</Key>")])
		}
		i := strings.Index(s, "<NextContinuationToken>")
		if i < 0 {
			break
		}
		token = s[i+23 : strings.Index(s, "</NextContinuationToken>")]
	}
	if strings.Join(keys, ",") != "data/aa/1,data/ab/2,data/ab/3" {
		t.Fatal(keys)
	}
	if c, _ := do(t, ts, "DELETE", "/bk/config", nil, nil, "SK"); c != 204 {
		t.Fatal(c)
	}
	if c, _ := do(t, ts, "HEAD", "/bk/config", nil, nil, "SK"); c != 404 {
		t.Fatal("deleted object still there")
	}
}

func TestRejectsBadRequests(t *testing.T) {
	_, ts := newServer(t)
	if c, _ := do(t, ts, "PUT", "/bk/x", []byte("a"), nil, "wrong"); c != 403 {
		t.Fatalf("wrong secret accepted: %d", c)
	}
	if c, _ := do(t, ts, "PUT", "/bk/a/../b", []byte("a"), nil, "SK"); c == 200 {
		t.Fatal("path traversal accepted")
	}
	// A body that does not match its signed hash.
	req, _ := http.NewRequest("PUT", ts.URL+"/bk/y", bytes.NewReader([]byte("tampered")))
	Sign(req, []byte("original"), "AK", "SK", "us-east-1", time.Now())
	resp, _ := http.DefaultClient.Do(req)
	if resp.StatusCode != 403 {
		t.Fatal("tampered body accepted")
	}
	// An old request.
	req, _ = http.NewRequest("GET", ts.URL+"/bk/y", nil)
	Sign(req, nil, "AK", "SK", "us-east-1", time.Now().Add(-time.Hour))
	resp, _ = http.DefaultClient.Do(req)
	if resp.StatusCode != 403 {
		t.Fatal("stale request accepted")
	}
}

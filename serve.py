#!/usr/bin/env python3
import http.server, os
os.chdir(os.path.dirname(os.path.abspath(__file__)))
print("Serving at http://localhost:3000")
http.server.HTTPServer(("", 3000), http.server.SimpleHTTPRequestHandler).serve_forever()

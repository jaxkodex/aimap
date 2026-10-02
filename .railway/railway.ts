import { defineRailway, preserve, project, service } from "railway/iac";

// This repository manages only its own resources in the environment. Other
// repositories export their own partial name.
// See https://docs.railway.com/infrastructure-as-code#multi-repo-projects
export const partial = "aimap";

export default defineRailway(() => {
  // One service runs everything: `aimap all` starts the ingest worker, the
  // classifier and the HTTP API as child processes, and exits (so Railway
  // restarts it) if any of them stops. Only one replica: each copy would also
  // poll IMAP. To classify faster, raise CLASSIFY_CONCURRENCY. Needs a public
  // domain, set on the service; Railway sets PORT.
  const aimap = service("aimap", {
    build: { builder: "DOCKERFILE", dockerfilePath: "Dockerfile" },
    preDeploy: "aimap migrate",
    start: "aimap all",
    healthcheck: "/healthz",
    deploy: {
      restartPolicyType: "ON_FAILURE",
      restartPolicyMaxRetries: 10,
      // Dashboard-configured resource limits; keep IaC from clearing them.
      // Three processes share these now.
      limitOverride: {
        containers: { cpu: 1, memoryBytes: 1000000000 },
      },
    },
    // Imported variables: kept out of source, remote values untouched
    env: {
      AIMAP_SECRET_KEY: preserve(),
      DATABASE_URL: preserve(),
      S3_ACCESS_KEY_ID: preserve(),
      S3_BUCKET: preserve(),
      S3_ENDPOINT_URL: preserve(),
      S3_REGION: preserve(),
      S3_SECRET_ACCESS_KEY: preserve(),
      JEV_API_KEY: preserve(),
      FIREBASE_PROJECT_ID: preserve(),
      AIMAP_ALLOWED_EMAILS: preserve(),
    },
  });

  return project("aimap", {
    resources: [aimap],
  });
});

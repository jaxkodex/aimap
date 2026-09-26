import { defineRailway, github, preserve, project, service } from "railway/iac";

// This repository manages only its own resources in the environment. Other
// repositories export their own partial name.
// See https://docs.railway.com/infrastructure-as-code#multi-repo-projects
export const partial = "aimap";

export default defineRailway(() => {
  const aimap = service("aimap", {
    // From legacy railway.toml [build]
    build: { builder: "DOCKERFILE", dockerfilePath: "Dockerfile" },
    preDeploy: "aimap migrate",
    // From legacy railway.toml [deploy]
    deploy: {
      restartPolicyType: "ON_FAILURE",
      restartPolicyMaxRetries: 10,
      // Dashboard-configured resource limits; keep IaC from clearing them
      limitOverride: {
        containers: { cpu: 0.5, memoryBytes: 500000000 },
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
    },
  });
  // Classifier worker: same image, different start command. Shares the bucket
  // and database with the ingest worker but nothing else, so it can be scaled
  // or restarted independently. No AIMAP_SECRET_KEY: it never opens the
  // accounts file.
  const classify = service("classify", {
    source: github("jaxkodex/aimap"),
    build: { builder: "DOCKERFILE", dockerfilePath: "Dockerfile" },
    preDeploy: "aimap migrate",
    start: "aimap classify",
    deploy: {
      restartPolicyType: "ON_FAILURE",
      restartPolicyMaxRetries: 10,
      limitOverride: {
        containers: { cpu: 0.5, memoryBytes: 500000000 },
      },
    },
  });

  return project("aimap", {
    resources: [aimap, classify],
  });
});

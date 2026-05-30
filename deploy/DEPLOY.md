# Deployment Guide

Deploy GeoLang to AWS in under 5 minutes.

## Architecture

```
┌─────────────┐     ┌───────────────┐     ┌─────────────────┐
│  CloudFront │────▶│  ALB (HTTP)   │────▶│  ECS Fargate    │
│  (CDN)      │     │  Health checks│     │  tiletopia:3000 │
└─────────────┘     └───────────────┘     └────────┬────────┘
                                                    │
                                          ┌─────────▼─────────┐
                                          │    S3 Bucket       │
                                          │  (tile storage)    │
                                          └───────────────────┘
```

**Components:**
| Service | Purpose | Cost (small) |
|---------|---------|-------------|
| ECS Fargate | Runs GeoLang server (0.5 vCPU, 1GB) | ~$15/mo |
| S3 | Tile + asset storage | ~$2/mo per 100GB |
| CloudFront | CDN for global tile delivery | ~$5/mo per 100GB transfer |
| ALB | Load balancing + health checks | ~$16/mo |
| CloudWatch | Logs + monitoring | ~$3/mo |
| **Total** | | **~$40/mo** |

## Prerequisites

- AWS CLI configured (`aws configure`)
- Terraform >= 1.5
- Docker

## Quick Start

```bash
# 1. Deploy infrastructure
cd deploy/terraform
cp terraform.tfvars.example terraform.tfvars
# Edit terraform.tfvars with your settings
terraform init
terraform apply

# 2. Build and push Docker image
ECR_URL=$(terraform output -raw ecr_repository_url)
aws ecr get-login-password | docker login --username AWS --password-stdin $ECR_URL
docker build -t $ECR_URL:latest ../..
docker push $ECR_URL:latest

# 3. Update terraform with image URL and redeploy
# Set container_image in terraform.tfvars to $ECR_URL:latest
terraform apply

# 4. Access your deployment
echo "API: https://$(terraform output -raw cloudfront_domain)/api/v1/health"
echo "Catalog: https://$(terraform output -raw cloudfront_domain)/api/v1/catalog"
```

## Scaling

```hcl
# terraform.tfvars

# Horizontal scaling (more containers)
desired_count = 3

# Vertical scaling (bigger containers)
cpu    = 1024  # 1 vCPU
memory = 2048  # 2 GB
```

## Custom Domain

1. Create an ACM certificate in us-east-1 (for CloudFront)
2. Add `domain_name = "tiles.yourdomain.com"` to terraform.tfvars
3. Point DNS CNAME to the CloudFront domain

## CI/CD

Add to your GitHub Actions workflow:

```yaml
- name: Deploy to AWS
  env:
    AWS_ACCESS_KEY_ID: ${{ secrets.AWS_ACCESS_KEY_ID }}
    AWS_SECRET_ACCESS_KEY: ${{ secrets.AWS_SECRET_ACCESS_KEY }}
  run: |
    # Build and push
    aws ecr get-login-password --region us-east-1 | docker login --username AWS --password-stdin $ECR_URL
    docker build -t $ECR_URL:${{ github.sha }} .
    docker push $ECR_URL:${{ github.sha }}
    # Deploy
    aws ecs update-service --cluster tiletopia-prod --service tiletopia-prod --force-new-deployment
```

## Local Development

```bash
# Run with Docker Compose (includes MinIO for local S3)
docker compose up

# Access:
# - API: http://localhost:3000
# - MinIO Console: http://localhost:9001 (tiletopia / tiletopia-dev)
```

## Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `TILETOPIA_PORT` | Server port | `3000` |
| `TILETOPIA_DATA_DIR` | Local tile storage path | `/data` |
| `RUST_LOG` | Log level | `info` |
| `AWS_S3_BUCKET` | S3 bucket for tile storage | — |
| `AWS_REGION` | AWS region | — |

## Monitoring

CloudWatch logs are available at `/ecs/tiletopia-prod`. Set up alerts for:

- ECS task health check failures
- High CPU/memory usage
- 5xx error rate from ALB
- S3 storage growth

## Costs at Scale

| Tier | Traffic | Containers | Storage | Cost |
|------|---------|-----------|---------|------|
| Starter | 10K req/day | 1× Fargate | 10 GB | ~$40/mo |
| Growth | 100K req/day | 2× Fargate | 100 GB | ~$100/mo |
| Enterprise | 1M+ req/day | 4× Fargate | 1 TB | ~$400/mo |

All tile serving is edge-cached via CloudFront, so even heavy read traffic is cheap.

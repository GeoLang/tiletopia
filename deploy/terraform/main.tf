# TileTopia AWS Deployment
#
# One-command deployment: terraform apply
#
# Architecture:
#   - ECS Fargate (serverless containers) — runs TileTopia server
#   - S3 — stores tile data + uploaded assets
#   - CloudFront — CDN for tile delivery (global edge caching)
#   - ALB — load balancer with health checks
#   - ECR — container registry
#   - CloudWatch — logs and monitoring
#
# Cost estimate: ~$30-50/month for small deployment (Fargate 0.25 vCPU)

terraform {
  required_version = ">= 1.5"
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.0"
    }
  }
}

provider "aws" {
  region = var.aws_region
}

# ─── Variables ────────────────────────────────────────────────────────────────

variable "aws_region" {
  description = "AWS region to deploy to"
  type        = string
  default     = "us-east-1"
}

variable "project_name" {
  description = "Project name (used for resource naming)"
  type        = string
  default     = "tiletopia"
}

variable "environment" {
  description = "Environment (dev, staging, prod)"
  type        = string
  default     = "prod"
}

variable "container_image" {
  description = "Docker image for TileTopia server (ECR URI or Docker Hub)"
  type        = string
  default     = "" # Set after first `docker push`
}

variable "domain_name" {
  description = "Custom domain name (optional, e.g., tiles.yourdomain.com)"
  type        = string
  default     = ""
}

variable "cpu" {
  description = "Fargate task CPU units (256 = 0.25 vCPU)"
  type        = number
  default     = 512
}

variable "memory" {
  description = "Fargate task memory in MB"
  type        = number
  default     = 1024
}

variable "desired_count" {
  description = "Number of running container instances"
  type        = number
  default     = 1
}

locals {
  name_prefix = "${var.project_name}-${var.environment}"
  tags = {
    Project     = var.project_name
    Environment = var.environment
    ManagedBy   = "terraform"
  }
}

# ─── Networking (VPC) ─────────────────────────────────────────────────────────

resource "aws_vpc" "main" {
  cidr_block           = "10.0.0.0/16"
  enable_dns_hostnames = true
  enable_dns_support   = true
  tags                 = merge(local.tags, { Name = "${local.name_prefix}-vpc" })
}

resource "aws_subnet" "public" {
  count                   = 2
  vpc_id                  = aws_vpc.main.id
  cidr_block              = "10.0.${count.index + 1}.0/24"
  availability_zone       = data.aws_availability_zones.available.names[count.index]
  map_public_ip_on_launch = true
  tags                    = merge(local.tags, { Name = "${local.name_prefix}-public-${count.index}" })
}

resource "aws_internet_gateway" "main" {
  vpc_id = aws_vpc.main.id
  tags   = merge(local.tags, { Name = "${local.name_prefix}-igw" })
}

resource "aws_route_table" "public" {
  vpc_id = aws_vpc.main.id
  route {
    cidr_block = "0.0.0.0/0"
    gateway_id = aws_internet_gateway.main.id
  }
  tags = merge(local.tags, { Name = "${local.name_prefix}-public-rt" })
}

resource "aws_route_table_association" "public" {
  count          = 2
  subnet_id      = aws_subnet.public[count.index].id
  route_table_id = aws_route_table.public.id
}

data "aws_availability_zones" "available" {
  state = "available"
}

# ─── S3 Bucket (Tile Storage) ────────────────────────────────────────────────

resource "aws_s3_bucket" "tiles" {
  bucket = "${local.name_prefix}-tiles"
  tags   = local.tags
}

resource "aws_s3_bucket_versioning" "tiles" {
  bucket = aws_s3_bucket.tiles.id
  versioning_configuration {
    status = "Enabled"
  }
}

resource "aws_s3_bucket_server_side_encryption_configuration" "tiles" {
  bucket = aws_s3_bucket.tiles.id
  rule {
    apply_server_side_encryption_by_default {
      sse_algorithm = "AES256"
    }
  }
}

resource "aws_s3_bucket_public_access_block" "tiles" {
  bucket                  = aws_s3_bucket.tiles.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_lifecycle_configuration" "tiles" {
  bucket = aws_s3_bucket.tiles.id
  rule {
    id     = "archive-old-tiles"
    status = "Enabled"
    transition {
      days          = 90
      storage_class = "INTELLIGENT_TIERING"
    }
  }
}

# ─── CloudFront CDN ──────────────────────────────────────────────────────────

resource "aws_cloudfront_distribution" "tiles_cdn" {
  enabled             = true
  is_ipv6_enabled     = true
  comment             = "${local.name_prefix} tile CDN"
  default_root_object = ""
  price_class         = "PriceClass_100" # US, Canada, Europe only (cheapest)

  origin {
    domain_name = aws_lb.main.dns_name
    origin_id   = "alb"
    custom_origin_config {
      http_port              = 80
      https_port             = 443
      origin_protocol_policy = "http-only"
      origin_ssl_protocols   = ["TLSv1.2"]
    }
  }

  default_cache_behavior {
    allowed_methods        = ["GET", "HEAD", "OPTIONS", "PUT", "POST", "PATCH", "DELETE"]
    cached_methods         = ["GET", "HEAD"]
    target_origin_id       = "alb"
    viewer_protocol_policy = "redirect-to-https"

    forwarded_values {
      query_string = true
      headers      = ["Authorization", "Origin"]
      cookies {
        forward = "none"
      }
    }

    min_ttl     = 0
    default_ttl = 3600  # 1 hour for API responses
    max_ttl     = 86400 # 24 hours max
  }

  # Cache terrain tiles aggressively (they rarely change)
  ordered_cache_behavior {
    path_pattern           = "/api/v1/terrain/*"
    allowed_methods        = ["GET", "HEAD"]
    cached_methods         = ["GET", "HEAD"]
    target_origin_id       = "alb"
    viewer_protocol_policy = "redirect-to-https"

    forwarded_values {
      query_string = false
      cookies {
        forward = "none"
      }
    }

    min_ttl     = 86400   # 1 day minimum
    default_ttl = 604800  # 7 days
    max_ttl     = 2592000 # 30 days
    compress    = true
  }

  # Catalog: reads need a JWT and /catalog/{id}/add is a POST, so the whole path
  # is uncached and Authorization goes to the origin. TTL 0 keeps an authorized
  # response from being replayed to another token or after that token expires.
  ordered_cache_behavior {
    path_pattern           = "/api/v1/catalog*"
    allowed_methods        = ["GET", "HEAD", "OPTIONS", "PUT", "POST", "PATCH", "DELETE"]
    cached_methods         = ["GET", "HEAD"]
    target_origin_id       = "alb"
    viewer_protocol_policy = "redirect-to-https"

    forwarded_values {
      query_string = true
      headers      = ["Authorization", "Origin"]
      cookies {
        forward = "none"
      }
    }

    min_ttl     = 0
    default_ttl = 0
    max_ttl     = 0
    compress    = true
  }

  restrictions {
    geo_restriction {
      restriction_type = "none"
    }
  }

  viewer_certificate {
    cloudfront_default_certificate = true
  }

  tags = local.tags
}

# ─── ECR (Container Registry) ────────────────────────────────────────────────

resource "aws_ecr_repository" "tiletopia" {
  name                 = local.name_prefix
  image_tag_mutability = "MUTABLE"
  force_delete         = true

  image_scanning_configuration {
    scan_on_push = true
  }

  tags = local.tags
}

resource "aws_ecr_lifecycle_policy" "tiletopia" {
  repository = aws_ecr_repository.tiletopia.name
  policy = jsonencode({
    rules = [{
      rulePriority = 1
      description  = "Keep last 10 images"
      selection = {
        tagStatus   = "any"
        countType   = "imageCountMoreThan"
        countNumber = 10
      }
      action = {
        type = "expire"
      }
    }]
  })
}

# ─── ECS Cluster & Service ───────────────────────────────────────────────────

resource "aws_ecs_cluster" "main" {
  name = local.name_prefix

  setting {
    name  = "containerInsights"
    value = "enabled"
  }

  tags = local.tags
}

resource "aws_ecs_task_definition" "tiletopia" {
  family                   = local.name_prefix
  network_mode             = "awsvpc"
  requires_compatibilities = ["FARGATE"]
  cpu                      = var.cpu
  memory                   = var.memory
  execution_role_arn       = aws_iam_role.ecs_execution.arn
  task_role_arn            = aws_iam_role.ecs_task.arn

  container_definitions = jsonencode([{
    name  = "tiletopia"
    image = var.container_image != "" ? var.container_image : "${aws_ecr_repository.tiletopia.repository_url}:latest"
    portMappings = [{
      containerPort = 3000
      protocol      = "tcp"
    }]
    environment = [
      { name = "TILETOPIA_PORT", value = "3000" },
      { name = "TILETOPIA_DATA_DIR", value = "/data" },
      { name = "RUST_LOG", value = "info,tiletopia=debug" },
      { name = "AWS_S3_BUCKET", value = aws_s3_bucket.tiles.id },
      { name = "AWS_REGION", value = var.aws_region },
    ]
    logConfiguration = {
      logDriver = "awslogs"
      options = {
        "awslogs-group"         = aws_cloudwatch_log_group.tiletopia.name
        "awslogs-region"        = var.aws_region
        "awslogs-stream-prefix" = "ecs"
      }
    }
    healthCheck = {
      command     = ["CMD-SHELL", "curl -f http://localhost:3000/api/v1/health || exit 1"]
      interval    = 30
      timeout     = 5
      retries     = 3
      startPeriod = 10
    }
  }])

  tags = local.tags
}

resource "aws_ecs_service" "tiletopia" {
  name            = local.name_prefix
  cluster         = aws_ecs_cluster.main.id
  task_definition = aws_ecs_task_definition.tiletopia.arn
  desired_count   = var.desired_count
  launch_type     = "FARGATE"

  network_configuration {
    subnets          = aws_subnet.public[*].id
    security_groups  = [aws_security_group.ecs.id]
    assign_public_ip = true
  }

  load_balancer {
    target_group_arn = aws_lb_target_group.tiletopia.arn
    container_name   = "tiletopia"
    container_port   = 3000
  }

  depends_on = [aws_lb_listener.http]
  tags       = local.tags
}

# ─── Application Load Balancer ────────────────────────────────────────────────

resource "aws_lb" "main" {
  name               = "${local.name_prefix}-alb"
  internal           = false
  load_balancer_type = "application"
  security_groups    = [aws_security_group.alb.id]
  subnets            = aws_subnet.public[*].id
  tags               = local.tags
}

resource "aws_lb_target_group" "tiletopia" {
  name        = "${local.name_prefix}-tg"
  port        = 3000
  protocol    = "HTTP"
  vpc_id      = aws_vpc.main.id
  target_type = "ip"

  health_check {
    enabled             = true
    path                = "/api/v1/health"
    healthy_threshold   = 2
    unhealthy_threshold = 3
    timeout             = 5
    interval            = 30
  }

  tags = local.tags
}

resource "aws_lb_listener" "http" {
  load_balancer_arn = aws_lb.main.arn
  port              = 80
  protocol          = "HTTP"

  default_action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.tiletopia.arn
  }
}

# ─── Security Groups ─────────────────────────────────────────────────────────

resource "aws_security_group" "alb" {
  name_prefix = "${local.name_prefix}-alb-"
  vpc_id      = aws_vpc.main.id

  ingress {
    from_port   = 80
    to_port     = 80
    protocol    = "tcp"
    cidr_blocks = ["0.0.0.0/0"]
  }

  ingress {
    from_port   = 443
    to_port     = 443
    protocol    = "tcp"
    cidr_blocks = ["0.0.0.0/0"]
  }

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }

  tags = local.tags
}

resource "aws_security_group" "ecs" {
  name_prefix = "${local.name_prefix}-ecs-"
  vpc_id      = aws_vpc.main.id

  ingress {
    from_port       = 3000
    to_port         = 3000
    protocol        = "tcp"
    security_groups = [aws_security_group.alb.id]
  }

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }

  tags = local.tags
}

# ─── IAM Roles ───────────────────────────────────────────────────────────────

resource "aws_iam_role" "ecs_execution" {
  name = "${local.name_prefix}-ecs-execution"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Action    = "sts:AssumeRole"
      Effect    = "Allow"
      Principal = { Service = "ecs-tasks.amazonaws.com" }
    }]
  })
  tags = local.tags
}

resource "aws_iam_role_policy_attachment" "ecs_execution" {
  role       = aws_iam_role.ecs_execution.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AmazonECSTaskExecutionRolePolicy"
}

resource "aws_iam_role" "ecs_task" {
  name = "${local.name_prefix}-ecs-task"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Action    = "sts:AssumeRole"
      Effect    = "Allow"
      Principal = { Service = "ecs-tasks.amazonaws.com" }
    }]
  })
  tags = local.tags
}

# S3 access for tile storage
resource "aws_iam_role_policy" "ecs_s3" {
  name = "${local.name_prefix}-s3-access"
  role = aws_iam_role.ecs_task.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect = "Allow"
      Action = [
        "s3:GetObject",
        "s3:PutObject",
        "s3:DeleteObject",
        "s3:ListBucket",
      ]
      Resource = [
        aws_s3_bucket.tiles.arn,
        "${aws_s3_bucket.tiles.arn}/*",
      ]
    }]
  })
}

# ─── CloudWatch Logs ──────────────────────────────────────────────────────────

resource "aws_cloudwatch_log_group" "tiletopia" {
  name              = "/ecs/${local.name_prefix}"
  retention_in_days = 30
  tags              = local.tags
}

# ─── Outputs ──────────────────────────────────────────────────────────────────

output "alb_dns_name" {
  description = "ALB DNS name (use this to access the API)"
  value       = aws_lb.main.dns_name
}

output "cloudfront_domain" {
  description = "CloudFront CDN domain (use this for tile delivery)"
  value       = aws_cloudfront_distribution.tiles_cdn.domain_name
}

output "ecr_repository_url" {
  description = "ECR repository URL (push Docker images here)"
  value       = aws_ecr_repository.tiletopia.repository_url
}

output "s3_bucket_name" {
  description = "S3 bucket for tile storage"
  value       = aws_s3_bucket.tiles.id
}

output "deploy_commands" {
  description = "Commands to deploy after terraform apply"
  value       = <<-EOT
    # 1. Build and push Docker image:
    aws ecr get-login-password --region ${var.aws_region} | docker login --username AWS --password-stdin ${aws_ecr_repository.tiletopia.repository_url}
    docker build -t ${aws_ecr_repository.tiletopia.repository_url}:latest .
    docker push ${aws_ecr_repository.tiletopia.repository_url}:latest

    # 2. Force new deployment:
    aws ecs update-service --cluster ${local.name_prefix} --service ${local.name_prefix} --force-new-deployment

    # 3. Access your API:
    curl https://${aws_cloudfront_distribution.tiles_cdn.domain_name}/api/v1/health
  EOT
}

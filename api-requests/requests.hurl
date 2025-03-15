# Get root
GET {{host}}

# Create monitor for endpoint
POST {{host}}/monitor

{
    "endpoint": "{{server}}",
    "discord_webhook": "{{discord_webhook}}"
}

# Get monitor stats for endpoint
GET {{host}}/monitor?endpoint={{server}}
